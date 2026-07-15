# Changelog

# 0.2.101 — 2026-07-13

## Features

- **grok inspect** now shows effective compatibility settings for Cursor, Claude, and Codex sessions.
- **New setting** "Match display refresh rate" lets high-refresh displays run the TUI at native cadence.

## Bug Fixes

- **Parked subagent status** no longer duplicates or interleaves incorrectly in scrollback.
- **Status line** during waits now shows elapsed time before the queued-message hint.
- **Queued messages sent with Enter** now appear immediately instead of vanishing briefly.
- **Resume hint** after quitting minimal mode now prints the correct grok --minimal --resume command.
- **Rate-limit messages** now correctly direct API-key users to team plans instead of personal upgrades.


# 0.2.100 — 2026-07-13

## Features

- **Session picker** now discovers and resumes recent Claude Code, Codex, and Cursor sessions.
- **Welcome screen** now offers a one-click resume nudge for recent Claude, Codex, or Cursor sessions.

## Bug Fixes

- **Web fetch tool** preserves full truncated page content as readable artifacts instead of discarding it.
- **Multiline mode** now correctly sends the top queued message on empty Enter when a turn is running.
- **Queued commands** no longer disappear or delay when pressing Enter twice quickly during a running turn.
- **Minimal mode** text is now readable on dark terminals with proper contrast and highlighted user prompts.
- **Grok no longer crashes** when printing resume hints after the terminal pane has closed.
- **Long-running turns** with multiple waits now show updated status markers in the transcript instead of appearing stuck.
- **Claude and Cursor hooks** are now correctly disabled at session start when disabled in config.


# 0.2.99 — 2026-07-12

## Features

- **Multiline input** now works on the agent dashboard the same way it does in regular sessions.
- **PageUp and PageDown** now scroll the conversation while the prompt is focused.
- **Keyboard Shortcuts** modal now follows Vim mode navigation keys when enabled.


# 0.2.98 — 2026-07-12

## Breaking Changes

- You can now pin authentication to API key or OIDC in config.toml; the unpinned method is no longer tried automatically.

## Features

- **`/context`** now shows token costs for skills and MCP servers.
- **`env_key`** in config now accepts an array of environment variable names.
- Linux middle-click paste from the primary selection now works; clipboard errors are handled more reliably.
- **/terminal-setup** now shows your terminal's color support level and which themes are available.
- **grok setup --json** prints your team's managed configuration without installing it.
- Messages you type while the model waits on tasks now stay queued; pressing Enter twice sends them immediately by cancelling the current turn.
- **How-to Guides** modal now shows a tip linking to Ask Grok above the footer shortcuts.
- **Subagent** `task` and `spawn_subagent` tools now accept an optional `model` parameter in the CLI.
- **Keyboard Shortcuts** modal now lists the paste key binding for images under the Input section.

## Bug Fixes

- A `pre_tool_use` deny now feeds the reason back so the model can retry instead of cancelling the turn.
- Plan mode now strictly rejects edits outside the plan file, even under always-approve.
- **Web search** and X search no longer fail when both a local function tool and the backend hosted tool are active.
- **Content-filter refusals** from providers now show an explanation instead of ending silently with no output.
- **SQLite databases** no longer cause bus errors on network filesystems such as NFS.
- **Resuming** a session that is already open now focuses the existing view instead of creating duplicates.
- **Turn completion** markers in scrollback now read "Worked for …" instead of "Turn completed in …".
- **/btw** loading spinner now animates correctly when the main session is idle.
- **Mid-turn** wait spinners now correctly show "Waiting on task output…" instead of Thinking.
- **Scrollbar thumb** is now visible in the oscura-midnight theme.
- Status messages for background work now end with a period.
- **Editing queued prompts** no longer freezes the terminal or duplicates text into the composer.


# 0.2.97 — 2026-07-11

## Features

- **Headless JSON output** now includes token usage and cost per prompt and session.
- **SDK turns** now expose detailed token usage and cost information via Turn.usage.
- **Double-click or Enter** on a previous user message now lets you edit and resubmit it directly from the transcript.
- **Text selection** in scrollback now works better when starting on chrome, gaps, or while scrolling.
- **Shell commands using `rg`** (ripgrep) no longer require permission prompts by default.
- **Voice mode** is now available for API-key sessions.
- **New environment variables** allow tuning scroll and draw cadence for high-refresh displays.

## Bug Fixes

- **Background tasks** started by the model in headless mode are now killed on exit instead of leaking.
- **Agent process leaks** on failed spawns and missing-stdio teardown are now prevented.
- **Parked turn markers** no longer appear after interjections and now count down as background tasks finish.
- **The /context** tool definitions line no longer shows the cryptic disclaimer suffix.
- **Terminal release** waits boundedly for the process to exit and repeated waits share the drain grace.
- **Fixed a crash** when the agent asked a question on narrow terminals.
- **Fixed misleading keyboard hints** in the /mcps panel in minimal mode.
- **Clipboard copy** now reports success correctly when using iTerm2 over SSH.
- **Fixed scroll/input conflicts** when plan approval appeared over an open edit block.
- **Fixed frequent MCP and skills reloads** that could freeze sessions on devboxes.
- **MCP servers using HTTP** (such as streamable HTTP MCP servers) now automatically recover from disconnects.
- **Next reset time** in /usage now shows in your local timezone instead of Pacific Time.


# 0.2.96 — 2026-07-10

## Features

- **System notifications** now carry structured kind/title/body for better rendering.
- **x.ai/pr/status** now reports whether an open PR is in the merge queue.
- **Compact mode** now activates automatically on very small terminals.
- **Up arrow** on an empty prompt now browses prompt history; `/history` searches it.
- **Stop hook runs** now appear inline on the turn-completed line instead of a separate block.
- **Subagent rows** now fold into verb-group headers and the tasks pane shows live activity labels.
- **Dashboard shortcuts** now advertise ? instead of Ctrl+. on terminals that cannot deliver the latter.
- **Double-clicking** scrollback while Text selection is fold/nav now shows a tip offering Ctrl+Y to enable Word select.
- **`grok worktree ls`** now works as a short alias for `grok worktree list`.
- **MCP tool output truncation** can now be set per-repo in `.grok/config.toml`.
- **Auto-send of queued follow-ups** during task waits can now be enabled fleet-wide via remote settings.
- **Welcome screen** now offers one-click resume of a recent Claude Code session via ctrl+u.

## Bug Fixes

- **Vim `l` key** now opens the selected agent detail view in the dashboard.
- **Terminal commands** with no args now run through a shell, matching the CLI.
- **Agent teardown** no longer crashes on slim Linux images that lack the ps command.
- **Esc** now dismisses an open /btw panel before backing out of a dashboard overlay.
- **Resumed grok.com chats** now use the conversation's last model instead of the gateway default.
- **JetBrains terminals on Windows** now default to minimal mode to avoid raw mouse-report leaks in the prompt.
- **Skill token highlights** now survive line wraps and the slash menu opens when typing / before existing text.
- **Truncated or tiny images** are now dropped before sending and previously poisoned sessions self-heal on restart.
- **Session switch hints** after `/new` or fork now show the working command in minimal mode.
- **Progress bars** in Ghostty and WezTerm now stop correctly for parked task waits.
- **`/effort`** now rejects levels the current model does not support instead of sending a bad value to the API.
- **`/recap`** on a fresh session now says "No messages yet" instead of failing.
- **Monitor and system messages** no longer appear as user prompts when resuming old sessions.
- **`/rewind`** completion now appears as a brief toast instead of a permanent transcript line.
- **Auto recaps** no longer appear under a newer user message when you start typing again.
- **Authentication retries** after token refresh no longer hang for minutes or days.
- **Text selection tip** no longer appears on the first double-click or on non-assistant blocks.
- **Skill slash commands** queued while a turn runs can now be sent immediately with Enter or the interject chord.
- **Drag text selection** now works inside the dashboard dispatch input box.
- **Multi-line paste chips** in dashboard inputs now support preview and expand like the main prompt.
- **Live previews** now always fetch the latest content without browser or CDN caching.


# 0.2.95 — 2026-07-09

## Features

- **Teams** can now ship default allowed commands via managed_config.toml (user deny rules still win).
- **Mid-turn interjections** now appear as normal user prompts (❯) instead of a separate cyan block.

## Bug Fixes

- **IME text input in Otty** no longer attaches unrelated clipboard images on every character.
- **Rewind** now fully removes the selected turn from both scrollback and the model's conversation history.
- **Queued prompts** now abort long blocking waits instead of waiting for the full timeout.
- **File links and media** now work for worktree sessions under ~/.grok/worktrees/.
- **Collapsed Read/Edit tool rows** now show only the filename instead of long absolute paths.
- **Clipboard copies on Wayland** now succeed even when the terminal loses focus mid-copy.
- **User messages queued** behind an auto-wake turn are no longer lost when the user presses Ctrl+C.
- **Slash completion** now shows sibling skills that share a frontmatter name and correctly sizes wrapped descriptions.
- **Single tool calls** that belong to a verb group now collapse into an aggregated header row.
- **Fixed sessions** that became permanently stuck after tool-use history corruption.
- **/always-approve** and **/auto** now toggle their mode on and off when run repeatedly.
- **Terminal command cards** on grok.com now correctly settle after foreground bash tasks.
- **Copy failure** toast now recommends trying /minimal for native terminal rendering.

## Performance

- **File watching** on Linux now uses far fewer system resources for large projects with many dependencies.


# 0.2.94 — 2026-07-09

## Features

- **/sessions** now opens the Agent Dashboard instead of a separate picker.
- **New /goal <objective>** slash command** is now available when the workspace supports it.
- **grok inspect** now lists skills from [skills].paths and correctly labels bundled vs user skills.
- **--minimal** and **--fullscreen** choices are now remembered for future plain grok launches.

## Bug Fixes

- **Queued bash commands** promoted at turn end now render their output instead of disappearing.
- **Xcode / Foundation ACP clients** can now drive grok agent stdio without silent parse drops on session/* calls.
- **read_file** now returns full single-line content (minified JSON, large dumps) instead of silently clipping at 2000 characters.
- **Background task** command preambles with newlines now render on separate lines instead of collapsing.
- **Text selections** now highlight uniformly even over inline code, links, and syntax-colored spans.
- **grok --minimal** now supports native drag-select on classic Windows conhost terminals.
- Skill tokens such as /pr-workflow are now highlighted teal when used mid-sentence.
- Fixed a crash when a filtered list shrinks while the filter is active.
- Scroll lines and scroll speed settings now support fine unit-step adjustments.
- Project-specific Claude plugins are no longer visible outside their project directory.
- First prompt no longer stalls for many seconds on large repositories while the filesystem watcher starts.


# 0.2.93 — 2026-07-08

## Breaking Changes

- **Esc** no longer cancels a running turn; use **Ctrl+C** instead. Double-Esc rewind now works while focused on scrollback.

## Features

- MCP permission prompts now show the planned arguments so you can judge what the tool will actually do.
- The "Managed by grok.com" link in the Extensions modal is now clickable and underlined.
- Dragging inside rendered markdown tables now selects whole cells or rectangular ranges and copies as TSV.
- Shift+Tab now goes straight to Plan mode when the plan-mode tip is showing.

## Bug Fixes

- **grok --minimal** now aligns the prompt, status bar, and messages flush-left with the welcome card.
- **/plugins** no longer lists never-installed Claude marketplace entries and now groups plugins by their real source.
- Successful image compression no longer leaves a permanent line in the transcript.
- **--no-ask-user** now also disables ask_user_question for subagents.
- **--no-ask-user** now also disables ask_user_question for subagents.
- **Fixed a crash** shortly after launch on some systems caused by the telemetry exporter.


# 0.2.92 — 2026-07-08

## Features

- **/minimal** and **/fullscreen** commands let you switch the current session between minimal and fullscreen modes.
- **ask_user_question tool** can now be enabled or disabled via config.toml, environment variables, or remote flags while defaulting to on.

## Bug Fixes

- **User-run shell commands** now display their complete output after finishing instead of silently dropping middle lines.
- **Edit tool output** now correctly highlights multi-line strings and scopes that previously spilled across hunks.
- **Always allow** grants for MCP, web_fetch and bash now take effect immediately in auto mode without re-prompting.
- **Cmd/Ctrl+click** on bare http(s) links now opens only once on Warp terminals.
- **Cmd/Ctrl+click** now works on imagine media paths and URLs that wrap across multiple terminal rows.
- **grok update** on Windows no longer fails when a previous.old executable is still running.

## Performance

- **Pasting images** on macOS is now ~65× faster by reading the pasteboard directly instead of via osascript.


# 0.2.91 — 2026-07-07

## Bug Fixes

- **Voice dictation indicator** and stop button now remain visible and clickable during plan mode review.
- **New Worktree dialog** now expands to show long names and scrolls with a leading … when the terminal is narrow.

# 0.2.90 — 2026-07-07

## Features

- **New /minimal and /fullscreen slash commands** let you switch the current session between minimal and fullscreen modes without quitting.
- **Session titles** from /rename now appear on the prompt box border after resume.
- **grok models** banner now correctly reports per-model API keys and deployment keys.
- MCP tool output size limit is now configurable via environment variable, config.toml, or remote settings (default unchanged).
- Chat conversations listed in the unified sidebar can now be renamed or deleted from the desktop app.
- You can now add a local directory as a plugin marketplace source with `grok plugin marketplace add`.
- **Auto permission mode** now prompts far less often on routine development commands.
- Short media paths the model prints (images/1.jpg) are now clickable and open the file.
- **Preview** now prefers common dev ports like 8080 when multiple HTTP servers are detected.

## Bug Fixes

- **Model list now refreshes** after upgrading from a free to a paid subscription tier.
- **Extensions modal** now shows clearer enable/disable and install hints that match what each key actually does.
- **Folder trust** no longer prompts for or scans the entire home directory when it is a git repo.
- **Code blocks** no longer lose their background shading on the final line of unterminated fences.
- **Plan mode** now activates immediately when toggled during an active turn instead of waiting for the next prompt.
- Clicking back into the terminal window now focuses the prompt immediately when a permission or plan-approval panel is waiting.
- Paths the model emits inside quotes no longer end with literal backslash-n characters and cause file-not-found errors.
- **Next reset** time shown by /usage is now correct during daylight saving time.
- The ask-user-question tool now waits up to 30 minutes by default before timing out.
- The free-usage paywall no longer offers a Try Again button.
- Inline images no longer bleed through when entering or leaving the fullscreen subagent view.
- The [Open Image] button under generated media is now colored like a link.
- **Preview routing** now auto-selects well-known ports like 8080 even with framework signals on obscure ports.
- **Login screen** now centers the authentication URL when it fits on one line.
- **Enter key** now queues follow-ups by default while the agent waits on task output.


# 0.2.89 — 2026-07-07

## Features

- **Voice dictation** now works on Linux (requires pipewire, pulseaudio-utils or alsa-utils).
- **New /auto slash command** switches to classifier permission mode; the menu now shows only the other mode.
- **--effort** and **--reasoning-effort** are now interchangeable CLI flags for setting reasoning effort.
- **Image edits** now use the higher-quality Imagine model for better output.

## Bug Fixes

- **Try Again** on the free-usage paywall now correctly resubmits after rate-limit retries.
- **Cursor** now respects your terminal's default blink style instead of always blinking.
- **Skill commands** in scrollback now highlight only the command name, not the arguments.
- **Plan files** now default to.grok/plan.md to match Grok conventions.
- **LaTeX math** renders correctly for display equations and complex subscripts.
- **Queue hint** in the terminal no longer shows incorrect bold text on part of the message.

## Performance

- **Git operations** like rebase no longer cause long pauses from repeated full-repo scans.


# 0.2.88 — 2026-07-06

## Features

- **Scrolling** feels smoother with better trackpad and wheel handling plus configurable speed and mode.
- **Session search** now returns tighter multi-word results and handles filenames and plurals better.
- **Session picker** now always searches conversation content for queries of two or more characters.
- **Tool call grouping** is now enabled by default, folding consecutive reads and searches into single rows.
- **Plugins tab** now supports `u` to update the selected plugin and shows non-blocking success feedback.
- **Reasoning effort** used for a session is now recorded in summary.json and conversation history.

## Bug Fixes

- **Session content search** now correctly indexes messages containing escape sequences like newlines and quotes.
- **Formatted links** now keep their link color when wrapped in bold, italic, or strikethrough.
- **Resuming sessions** no longer fails permanently when history files contain corrupted lines from interrupted writes.


# 0.2.87 — 2026-07-05

## Features

- **Subscription upgrades** are now detected automatically without restarting the CLI.
- **Bash permission prompts** now offer a "Never allow" choice that persists the deny rule.
- **New `/docs` slash command** opens How-to Guides picker, browses web docs, or jumps directly to a guide by title.
- **Per-model reasoning effort menus** are now configurable from the server and config.toml without a client release.
- **Finished thinking blocks** now fold into grouped tool-call rows when group_tool_verbs is enabled.

## Bug Fixes

- **--minimal** mode now always uses your terminal's own colors so text stays readable on any background.
- **Invalid fields** in [model.*] config blocks no longer cause the whole model to disappear from the picker.
- **File tools** no longer target paths with literal trailing newlines or whitespace from model output.
- **Fixed interactions** in no-freeform question modals so clicks below the last option no longer enter input mode.
- **Background tasks** now correctly wake the agent after a cancelled blocking wait instead of staying idle.
- **Copying quoted text** from rendered responses no longer includes the quote bar prefix in pretty mode.

# 0.2.86 — 2026-07-04

## Features

- **Voice language** setting now lets you pick speech-to-text language (including System) in the Editor settings.
- **Tab autocomplete** now suggests your next prompt as ghost text after each turn.
- **/usage** (and /cost) is now hidden for free and X Basic personal accounts.
- **Media generation** no longer hits per-session file limits; image and video byte budgets increased.

## Bug Fixes

- **--minimal** flag now shows in `grok --help`.
- **Session resume notifications** no longer appear when a workspace boots for the first time.
- **Claude-style Bash(cmd:*)** permission rules are now correctly translated to prefix matches.


# 0.2.85 — 2026-07-03

## Features

- **Pressing Enter** on an empty prompt now sends the top queued follow-up immediately while a turn is running.
- **Tool call grouping** can now be enabled via config.toml or settings to collapse consecutive read/search/list calls.
- **Consecutive tool calls** of the same kind can now be folded into a single row when group_tool_verbs is enabled.
- **Subagent conversations** now receive the same type-specific instructions in gateway chat as in the CLI.
- **Scheduled automation tasks** now show their header panel correctly in gateway chat sessions.
- **Promotional announcements** now appear with clickable CTAs and can be non-dismissible.
- **Subagents** now run in the background by default unless explicitly set to false.

## Bug Fixes

- **Permission prompts** now correctly wrap long bash commands while preserving structure and quotes.
- **Claude Code settings** with permissions.defaultMode are now correctly honored.
- **Project skills and commands** are now discovered even when their directories are gitignored.
- **Inline LaTeX math** with padding spaces now renders correctly instead of showing raw dollar signs.
- **Manual /recap** now works on the same turn and long auto recaps are hidden from view while still saved.
- **Always allow** for common commands like ls and git status now remembers just the command instead of extra arguments.


# 0.2.84 — 2026-07-03

## Features

- **Announcements** now update live during active sessions without restart or `/new`.
- **Hiding** an announcement no longer suppresses later criticals; new ones reappear automatically.
- **run_terminal_cmd** now requires a one-sentence `description` rationale in every invocation.
- **ask_user_question** timeout policy is now configurable in config.toml and `/settings`.
- **Ask-Question timeout** can now be toggled from `/settings` (Agent & Approval).
- **Thinking/reasoning blocks** are now shown by default while the model is working.
- **Critical announcements** now show a red title with a clickable [hide] button and aligned message.
- **Added remote_fetch option** under [features] in config.toml to disable all backend catalog and settings fetches for air-gapped environments.

## Bug Fixes

- **Images** pasted or read from GIF, BMP or TIFF files are now automatically converted so they work with image generation.
- **Queue panel** now shows action buttons on hover and the status bar displays a compact done/total task count.
- **Hook matchers** now correctly see the real MCP tool name instead of the internal dispatcher name.
- **Copy** now succeeds when running inside containers even when the terminal brand cannot be detected.
- **Tool result previews** no longer paint opaque panels in `grok --minimal`.
- **grok wrap** now correctly handles quoted strings and shell aliases.
- **Text selection** settings now correctly honor explicit keep_text_selection values even when legacy keys remain.
- **Fixed a freeze** that could occur when editing and sending the last message in the queue.
- **Fixed a startup crash** on minimal Linux systems lacking system CA certificates.

## Performance

- **Grep** now stops early on broad searches, returning faster results with far less memory use.
- **Idle CPU and memory** usage after long sessions or resume is now dramatically lower.


# 0.2.83 — 2026-07-02

## Features

- **Critical announcements** now appear in a top banner during active sessions with a hide command.
- **Pasting the same text again** next to a paste chip now expands the chip into editable text instead of duplicating it.
- **Paste preview** now shows a hint explaining how to expand the chip.


# 0.2.82 — 2026-07-02

## Features

- **Managed connectors links** now include the team ID when opening from a team session.
- **AGENTS.md files** are now discovered and shown for workspace/hub sessions.
- **Chat conversation titles** from the gateway are now shown in the sidebar.
- New `/effort` slash command changes reasoning effort on the active model.
- Double-click a pasted text chip to expand it into editable text.

## Bug Fixes

- **Skill descriptions** are now recovered correctly even when frontmatter YAML is malformed.
- **[Esc] hint** in /btw panels now stays visible even on narrow terminals.
- **Background monitors** now wake the agent on natural exit the same way bash tasks do.
- Long option labels in question prompts are now always visible instead of disappearing when unfocused.
- Pasted text preview now appears immediately after inserting a paste chip.
- Hex color codes now render as colored dots with no extra space.
- Pressing voice on the welcome screen now starts a new session.

# 0.2.81 — 2026-07-01

## Features

- **Chat sessions** no longer send workspace binding hints that belong to the backend.
- **New stream transforms** let hosts hide, unwrap, or rewrite tool calls for display without affecting agent transcripts.
- **cancel()** now accepts a timeout so a stuck turn cannot hang the session forever.
- **Run tool blocks** now show the model's description as the main title when provided.
- **Hex color codes** in prose now render as colored dots on truecolor terminals.
- **New setting** "Show thinking blocks" controls whether agent reasoning is visible in the scrollback.
- **Spinners** now show the description or short command of the task being waited on when available.

## Bug Fixes

- **Fixed sessions** that remained stuck on the thinking indicator after the model finished responding.
- **Mermaid diagrams** now correctly display angle brackets and symbols instead of literal HTML entities in labels.
- **Recap requests** no longer trigger context-length 400 errors on long conversations.

## Performance

- **Grep searches** now time out after 20 seconds by default (60s on WSL) instead of always waiting 60s.

# 0.2.80 — 2026-07-01

## Features

- **Command timeouts** can now be configured per-session with a foreground-only ceiling.
- **Background tasks and TODO lists** now survive compaction and remain visible to the model.
- **Voice dictation** STT feature: uses Ctrl+Space or F8, with optional hold-to-talk on supported terminals.
- **Contextual hints** can now be toggled individually for undo, plan mode, and image input.

## Bug Fixes

- **Subagent dialogs** now reliably show full transcripts on open and reopen.
- **Recap blocks** now copy only the summary body, not the header label.
- **Vim navigation keys** now type into dashboard prompts; modals properly handle Esc/Left.

## Performance

- **Network connections** are now more resilient to proxy/LB drops.

# 0.2.79 — 2026-06-30

## Features

- **Contextual hints** now show shortcuts like plan mode or clipboard paste when relevant.
- **Graceful shutdowns** now allow interrupted turns to resume with a configurable pause budget.
- **Grok.com chat sessions** now integrate fully with the gateway bridge for model catalog and resume.

## Bug Fixes

- **Question prompts** now time out after 6 minutes instead of blocking forever.
- **Fixed a crash** that could occur during conversation integrity repairs while a turn was active.

## Performance

- **Compaction** can now run part of its work in the background before it blocks the session.

# 0.2.78 — 2026-06-30

## Features

- **Chat sessions show the grok.com model catalog** in the picker.

## Bug Fixes

- **Tabs pasted into the prompt** now align correctly with proper cursor positioning.
- **Pasting images into dashboard peek replies** now works and survives turn cancellation.
- **Links in /btw panels** are now clickable and highlight on hover.
- **Prompt history is now saved** even on fast Ctrl+C quit.
- **Stuck scrollback text selection** can now be cleared with Esc or any non-drag input.
- **LaTeX math now renders** inside markdown tables in the TUI.
- **Background shell commands** started by the agent are now cleaned up when the CLI exits.

## Performance

- **`grok update`** downloads have a longer timeout.


# 0.2.77 — 2026-06-30

## Features

- **Pasting images** from the local clipboard now works when running commands through `grok wrap`.
- **Turn status spinner** now shows what the agent is waiting on (response, subagent, task output, etc.).
- **Double-click word selection** is now a discoverable option in the Text selection setting and stays in sync with highlight behavior.

## Bug Fixes

- **Credit limit errors** now show clearer upgrade or buy-credits messaging based on billing type.


# 0.2.76 — 2026-06-30

## Features

- **Auto permission mode** is now added to the top of Shift+Tab cycles and enabled by default in settings.
- **grok agent stdio** now checks for updates in the background like other modes.

## Performance

- **Idle sessions** no longer send repeated empty frames to the terminal, reducing CPU usage in the terminal emulator.


# 0.2.75 — 2026-06-29

## Features

- **Prompt history** (Up arrow / Ctrl+R) now shows only the current session's prompts, with the newest selected at the bottom.

# 0.2.74 — 2026-06-29

## Features

- **Esc now cancels a running turn immediately**; double-Esc clears prompt or opens rewind when idle.
- **grok wrap** now shows copy success over SSH and suggests native drag-select when paste fails.

## Bug Fixes

- **Clipboard copy** now succeeds reliably on Wayland and KDE desktops instead of showing false positives.


# 0.2.73 — 2026-06-28

## Features

- **Keep text selection highlight** setting added so drag selections stay visible until dismissed.

## Bug Fixes

- **Doubled lines** after tab switches or focus changes in tmux or editor terminals are now healed.
- **Clipboard copy** now only shows success when the pasteboard actually received the text via a trusted path.


# 0.2.72 — 2026-06-28

## Bug Fixes

- **No longer triggers browser login** at startup when an API key is already configured for inference.


# 0.2.71 — 2026-06-27

## Bug Fixes

- **Fixed `grok agent stdio` hangs** on Windows when used with persistent clients such as VS Code.


# 0.2.70 — 2026-06-27

## Breaking Changes

- **Added `grok wrap`** to run any command with local clipboard support.

## Features

- **Ctrl+4** now toggles the prompt queue on local macOS VS Code terminals.

## Bug Fixes

- **Session recaps** (/recap and return-from-away) now show the full summary instead of being cut off mid-sentence.
- **Vim mode** now focuses the prompt when you press / on a brand-new empty session.
- **Fixed `grok agent stdio` startup hangs** on Windows when used with persistent clients such as VS Code or grok-desktop.
- **`/mcps` list** no longer shows stale disabled entries when managed gateway tools are enabled.
- **Mermaid diagrams opened via [Open Image]** now render at higher resolution instead of terminal size.
- **Pressing `r` in scrollback** no longer accidentally rewinds the session.
- **Shortcuts cheatsheet** now shows Ctrl+X on terminals that cannot deliver Ctrl+.
- **Folder trust prompts** no longer re-appear for every standalone worktree clone.
- **Reasoning effort** no longer silently resets from a user-chosen value after catalog refreshes.
- **Fixed clipboard copy** inside editor terminals nested in tmux by emitting plain OSC 52.

# 0.2.69 — 2026-06-26

## Features

- The agent dashboard now shows each agent's model and mode in the peek panel, lets you cycle modes with Shift+Tab, collapses the Inactive section by default, and hides older idle agents behind a "N more" row.
- Tool usage cards for search, directory listing, file deletion and glob now render as distinct typed cards instead of generic MCP entries.
- The keyboard shortcuts help now shows richer descriptions and correctly scrolls wrapped text in the detail view.
- You can now pass --json-schema to grok -p and receive a validated JSON object instead of free text.
- **Ctrl+L** now interjects mid-turn in VS Code, Cursor, Windsurf, and Zed terminals.

## Bug Fixes

- Local plugins installed from your home directory are now automatically refreshed when you start a session, so new agents or skills added to the source appear immediately.
- The /context command now reports the same number of tool definitions that are actually sent to the model.
- In vim mode the agent dashboard peek no longer steals keyboard focus from the list, so j and k keep moving between agents.
- **/sessions** on the agent dashboard no longer freezes the interface.
- **Dashboard** now focuses the overview list immediately when agents exist.

# 0.2.68 — 2026-06-26

## Features

- **MCP servers** from host integrations can now be added, replaced, or removed without restarting the session.
- **Agent-run terminal commands** now set `GROK_AGENT=1` so host tools can tell them apart from interactive shells.

## Bug Fixes

- **Attached images** are now saved to real disk paths so the model can read them in any terminal.
- **/resume** now selects the correct model when a saved model name is ambiguous.
- **Slash and completion menus** no longer crash if the terminal is resized while open.


# 0.2.67 — 2026-06-25

## Features

- **Added --json-schema** flag for headless mode to constrain model output to a supplied JSON Schema.
- **Idle detection** can now ignore background tasks when the env flag is set (off by default).

## Bug Fixes

- **Preview panes** no longer hibernate while actively viewed or polled.
- **Manual /rename** now persists correctly and appears in /session-info even after auto title generation or resume.

## Performance

- **Find and grep** now transparently use faster bfs and ugrep binaries when present in the harness.


# 0.2.66 — 2026-06-25

## Features

- **Custom sandbox profiles** can now kernel-deny specific files and directories for reads/writes.
- **Marketplace plugins** in subdirectories of a git repo can now be installed and loaded correctly.
- **Folder trust prompt** now appears before starting a session when the feature is enabled.
- **Preview panes** no longer hibernate while actively viewed.
- **Keyboard shortcuts help** now expands inline for individual entries instead of only sections.
- **Idle detection** can now ignore background tasks when the env flag is set (off by default).
- **Sandbox deny lists** now accept glob patterns like **/*.pem** in addition to exact paths.

## Bug Fixes

- **Local MCP servers** now auto-recover after disconnects or session expiry.
- **OIDC sessions** with XAI_API_KEY present no longer lose refresh on idle.
- **Inline video previews** now show an install command only when the package manager is on PATH.
- **list_dir** now reliably shows all immediate child directories even inside large monorepos.
- **Clicking a model** in the dashboard /model dropdown no longer opens the wrong session.
- **Strikethrough** now only applies to ~~double tildes~~; single ~tildes~ render literally.
- **Session cycling** with Ctrl+[ / ] now switches from the session you are currently viewing.
- **Prompt history** (Up / Ctrl+R) now shows the complete recent list instead of a scrambled partial one.
- **Authentication** now correctly prefers the session method when both API key and cached token are present.
- **xychart-beta** diagrams with category labels now render correctly as images.


# 0.2.65 — 2026-06-24

## Features

- **grok -w --ref <branch>** now creates worktrees based on the specified ref instead of HEAD.

## Bug Fixes

- **Unidentified Windows consoles** are now treated as Windows Terminal for capability decisions.
- **Esc** in the dashboard input now moves focus to the list without clearing your typed draft.
- **Copying** a tool header now copies just the path or command, not the Read/Run label.
- **Execute activity** lines and headers no longer repeat a redundant cd into the session directory.
- **Inline video previews** now show an install hint instead of a spinner when ffmpeg is missing.

## Performance

- **Headless and stdio sessions** no longer start unnecessary filesystem watchers, saving CPU and IO.
- **Scrolling** feels more responsive in VS Code, Cursor, and Windsurf integrated terminals.


# 0.2.64 — 2026-06-24

## Features

- **Dashboard** now displays the current directory and branch; click or press Ctrl+L to change location, or Ctrl+W to dispatch new agents into fresh git worktrees.
- **/recap** now appears as a collapsible tool-style block with a loading spinner while generating.

## Bug Fixes

- **Dashboard** arrow keys open agent details and exit overlays; closing an agent now selects the neighboring row.
- **/usage** command and credit warnings are now hidden for API-key authentication.
- **MCP servers** from your user config no longer appear labeled as project-scoped when running from your home directory.


# 0.2.63 — 2026-06-23

## Bug Fixes

- **Fixed hook matchers** so pipe-list and alias patterns no longer silently over-match unrelated tool names.


# 0.2.62 — 2026-06-23

## Features

- **Hosts can now register hooks** over the agent connection instead of only on-disk files.
- **Prompt and /usage warnings** now correctly reflect prepaid credits and auto top-up status.
- **Desktop clients can now detect** when a terminal is busy running a foreground process.
- **TODO list** remains visible to the model after compaction so it can continue working on pending items.
- **/recap** is now available by default — it generates a quick summary of your current session so you can catch up on what's happened so far.

## Bug Fixes

- **MCP server connections** no longer time out during slow cold starts of stdio servers that download dependencies.
- **File paths containing spaces** (e.g. macOS app bundles) are now correctly turned into clickable hyperlinks in the terminal.
- **Resume** now correctly picks the most recently active session instead of one that only had metadata updates.
- **/goal** slash command now appears in the menu on the welcome screen before any prompt is sent when the feature is enabled.
- **Session picker** no longer shows a stale row highlight when keyboard focus moves to the search bar.
- **Usage percentages** in /usage and warnings now match backend flooring and show pay-as-you-go limits when applicable.
- **Team accounts** can now list sessions after re-login; previously returned 403 on conversations API.


# 0.2.61 — 2026-06-22

## Features

- **Closing a terminal tab** with a running process now shows a confirmation dialog instead of killing it immediately.
- **/usage** now shows prepaid credits balance and auto top-up status.
- **Clipboard copy** on Wayland now also tries wl-copy; per-leg outcomes are now logged for diagnostics.
- **Goal mode toggles and limits** can now be set in config.toml under the [features] table.
- **All /goal options** (toggles, limits, role models) are now configurable together in a [goal] table.
- **Clipboard copies** from VS Code over SSH now warn when non-ASCII text may be garbled.

## Bug Fixes

- **Focus reports** no longer leak as literal text when split across reads over SSH.
- **--disable-web-search** now honored in grok -p and grok agent; auxiliary model routing respects catalog overrides.
- **Focus events** now fire correctly for SSH-split focus reports.
- **Boolean tool flags** now accept "true"/"false"/"yes"/"no"/1/0 strings and numbers in addition to native booleans.
- **Session last-active timestamps** and message counts no longer regress under concurrent writers.
- **iTerm2** now always uses text/metadata image fallback instead of broken OSC 1337 overlays.
- **Model switches** no longer leave the prompt queue stuck after a reconnect.
- **Closing a terminal tab** with a running process no longer shows a confirmation dialog.
- **Custom agent profiles** now correctly use the harness required by their pinned model.
- **Subagents** under custom profiles now adopt the correct harness from the parent's model.
- **Changelog and release-notes** modals now scroll with the mouse wheel and arrow keys.


# 0.2.60 — 2026-06-21

## Features

- **/resume** now shows sessions from your current working directory's repo at the top of the list.
- **Too-wide Mermaid diagrams** now show a hint below the fallback box pointing to the Open Image button.
- **Cancel behavior for running subagents** can now be set to always stop or always continue in config.toml.

## Bug Fixes

- **Compaction** no longer hangs indefinitely when the summarizer stream stalls after the server has finished.
- **Slash command completion** now shows consistent suggestions and remembers recently used commands.
- **Queued prompts** now reappear reliably after deleting the last item and re-queuing.
- **Headless sessions** no longer produce authentication error noise from unauthenticated MCP servers.
- **Mermaid flowchart labels** with long identifiers are now kept whole instead of being cut mid-word.
- **Cmd+Backspace** now deletes only from the cursor to the start of the line instead of clearing the whole prompt.
- **Inline Mermaid previews** now break long identifiers at word boundaries instead of mid-segment.
- **Signed git commits** no longer corrupt the TUI by letting pinentry draw over the screen.
- **Arrow keys** now move the prompt cursor or open history while a /btw answer panel is visible.
- **Long option descriptions** in question prompts now expand fully when the row is focused.

## Performance

- **Large MCP tool results** are now truncated inline and saved to disk to avoid unnecessary context compaction.


# 0.2.59 — 2026-06-19

## Bug Fixes

- **Session recaps** no longer display doubled labels and manual recap now correctly suppresses the next automatic recap.


# 0.2.58 — 2026-06-19

## Bug Fixes

- Terminal command output files are now capped at 5 GB during execution and truncated to 64 MB after the process exits.
- Interjection messages now display the actual user text instead of a generic header.
- The legacy `agent` command is now kept in sync with `grok` after running `grok update`.
- Headless (`grok -p`) runs now wait for background tasks and subagents to finish before exiting.


# 0.2.57

## Features

- Improved resilience to network blips during long responses by resuming instead of failing the turn.
- **`grok plugin install <name>`** now resolves plugins from registered marketplaces instead of only local paths.

## Bug Fixes

- Fixed cases where long-running conversation compaction could hang indefinitely.
- Notification hooks now fire only for real user-attention events and no longer trigger constantly during tool use.
- Fixed literal display of HTML entities such as &lt; and &gt; in responses and tool output.
- **Typing `[`** in the pager prompt no longer appears delayed.
- **Copy** now tries all available Linux clipboard tools so paste works reliably in more terminals.


# 0.2.56

## Features

- **resume_from** now continues a finished sub-agent in place instead of forking a new conversation.
- **grok sessions delete <id>** command now lets you permanently remove a session from the CLI.

## Bug Fixes

- **MCP server connections** no longer get torn down during rapid config reloads.
- **Stale leader processes** are now cleaned up when leader mode is disabled via config or remote settings.
- **Sandbox profile** is now preserved when resuming sessions so commands continue to work as before.
- **list_dir** now shows more relevant files when a large directory appears early in alphabetical order.
- **Cancel button** in turn status always shows [stop]; queue pane highlight now follows theme changes.
- **grok quit** no longer hangs when background git or network tasks are slow.
- The token count shown after auto-compaction now matches the context bar exactly.
- The git branch icon now renders correctly in iTerm2 without a Nerd Font.
- **list_dir** now gives clearer guidance when a directory is too large, using the actual tool names available in your session.
- **Ctrl+Enter** now sends the prompt when the agent is idle (same behavior as Enter).
- **resume_from** now correctly continues a sub-agent in the same working directory it was using before.
- Files with non-ASCII names (e.g. Chinese) no longer crash the session when plan mode checks for markdown.
- Session lists (welcome screen, /resume, grok sessions list) are now sorted by the same activity time shown in the UI.
- **Fixed bash tool failures** when models send numeric arguments such as timeout as JSON strings instead of numbers.
- **Prevented crashes** during bash command output streaming when building progress frames.
- **Disabled inline image rendering** on iTerm2 terminals where scrollback overlays cannot be supported.

## Performance

- Fast tools like grep now show as completed immediately even when other tools in the same round are still running.
- Long sessions that display inline images no longer grow to multi-GB memory usage.


# 0.2.55

## Features

- **Added option** to fully disable the hunk tracker via --hunk-tracker-mode, GROK_HUNK_TRACKER, or config.

## Bug Fixes

- **Windows install scripts** now download and run cleanly via irm | iex without spurious BOM errors.
- **Tables and wide content** no longer leave stray characters next to timestamps in the scrollback.
- **Mermaid diagrams** now render node labels cleanly without HTML tags or raw markdown syntax.
- **MCP servers using HTTP** now recover automatically after temporary connection drops instead of becoming permanently unavailable.
- **Very long sessions** can now scroll all the way to the bottom of the conversation history.


# 0.2.54

## Features

- **Rewind** now works end-to-end across conversation and file state with proper CAS handling.

## Bug Fixes

- **Git branch icons** now render correctly on Windows without Nerd Fonts.
- **Mermaid diagrams** now render inline without the model suggesting external viewers.
- MCP connection errors now show the actual failure reason to the model.
- MCP servers with noisy stdout no longer disconnect unexpectedly.
- **Usage warnings** now always display "Usage left: N%" instead of varying between "Free credits left" and "Credits left".
- **Window title** no longer flashes or oscillates during permission prompts while the terminal window is focused.

## Performance

- **Fixed pager freezes** and 100% CPU usage when rendering very long agent reasoning outputs with thousands of styled spans.


# 0.2.53

## Bug Fixes

- Minor bug fixes.

# 0.2.52

## Features

- **Tool auto-approval (YOLO)** state is now tracked end-to-end in server-side agent sessions.
- **ER diagrams** now render as entity boxes with attributes and relationships in the TUI.
- New "Respect manual folds" setting keeps hand-expanded blocks stable while content streams in.
- **Ctrl+X** now stops running turns or closes sessions from inside the agent detail view.
- **Grok** can now export usage metrics and events to your own OpenTelemetry collector when enabled.
- **WezTerm users** now receive guidance when Shift+Enter fails because kitty keyboard protocol is disabled.
- **Long-running sessions** now tell the model when the local calendar date changes past midnight.
- **Agent Dashboard** now works without leader mode and shows local idle sessions from disk.

## Bug Fixes

- **Fixed oversized session replay logs** that prevented large sessions from loading.
- **MCP server connections** no longer flood reconnects on repeated stream errors.
- **ZDR and team upload flags** are now populated immediately on login instead of only after background refresh.
- **Mermaid PNG export** now handles quoted cardinalities in class diagrams and readable ER rows on dark theme.
- **Skill catalog** no longer shows duplicate "Use when:" labels and check-work skill now prompts the model to read its instructions.
- Compaction now rejects overly-short summaries that would discard real conversation state.
- Background tasks no longer emit spurious failure messages when a session is resumed.
- **Fixed Windows path handling** so external tools and model prompts receive clean paths without \?\ prefixes.
- **Images and media** no longer remain visible when switching from an agent view to the dashboard.
- **Clipboard paste** (Ctrl+V) now works for images on pure Wayland sessions.
- **Modals** such as /sessions no longer crash on narrow terminals.
- **ptyctl resize** now correctly notifies the child process.
- **Concurrent updates** to the same version no longer fail with permission or EEXIST errors.
- **Mermaid diagrams** containing CJK or other non-Latin text now render correctly instead of tofu boxes.
- **`grok dashboard`** now reliably opens the dashboard instead of silently falling through to a normal session.
- **Sessions** no longer remain blocked forever after a transient model catalog outage during reconnect.
- **Cancel** no longer leaves the interface stuck on "Cancelling…" after lost responses during reconnects.
- **Forked sessions** now retain the parent's full pre-compaction transcripts instead of only the compacted summary.
- **web_fetch** errors on GitHub hosts now recommend using the gh CLI when internal access is blocked.
- **MCP server connections** no longer hang when stdio servers emit undecodable lines.
- **Ctrl+C cancels** now complete in under 50 ms instead of blocking for seconds.
- **Repeated varied edit failures** on one file no longer trigger doom-loop warnings or terminations.

## Performance

- **Compaction** now reuses cached prompt prefix instead of full prefill.

# 0.2.51

## Breaking Changes

- **`grok mcp add`** now accepts positional arguments (e.g. `grok mcp add filesystem -- npx...`), supports --scope project, and adds -e/-H flags for env/headers.

## Features

- **Mermaid flowcharts** now render subgraph blocks as titled frames with correct internal and cross-boundary edges.
- **Class diagrams** in Mermaid now render as proper UML boxes with attributes, methods and inheritance arrows instead of raw source.
- **Permission prompts** now accept a double-click on an option to submit it, matching the existing Enter and number-key shortcuts.
- **New /code-review slash command** now ships with the CLI and is always available.

## Bug Fixes

- **Plan mode exit reminders** no longer appear after the model has already started implementing the plan.
- **Expanded thinking blocks** in scrollback now remain expanded when the agent finishes them.
- **`grok update`** no longer downloads the same binary twice when multiple updaters or leader checks run concurrently.
- **Background task IDs** after /compact are now shown verbatim so the model can reference them correctly in later tool calls.
- **Typing /** while scrollback is focused now focuses the prompt and opens the slash-command dropdown.
- **Dashboard empty state** is now a single hint line; dispatch and peek placeholders appear only when unfocused.
- **Fixed memory leaks** that could cause the CLI to use tens of gigabytes during long sessions with many tool calls.
- **Login on SSH or headless machines** now tells you when the browser cannot be opened automatically and shows the URL to visit manually.
- **Fixed git clone failures** on Windows when the CLI tries to clone marketplace plugins into ~/.grok.

## Performance

- **Large code blocks** inside lists no longer cause multi-second UI stalls while streaming responses.


# 0.2.50

## Features

- **Mermaid flowcharts** now render edge crossings clearly instead of fusing unrelated connections.

## Bug Fixes

- **Sequence diagrams** with activate, autonumber, par, and more now render instead of showing parse errors.
- **MCP servers menu** and slash commands now work when starting grok outside a project directory.
- **Ctrl+W** in the prompt now deletes whole words like bash instead of stopping at punctuation.
- **Login** no longer quits when an authentication code contains the letter q.


# 0.2.49

## Features

- Marketplace plugin listings now show skills, MCP servers, and commands when the catalog is published.
- Mermaid flowcharts now render with fewer avoidable edge crossings.
- **stateDiagram** mermaid blocks now render as Unicode diagrams instead of source fallback.

## Bug Fixes

- **Skill reloads** no longer corrupt active tool calls or produce duplicate results in the conversation.
- **grok --resume** now correctly finds the real session instead of failing on empty image-only folders.
- Pasted images and relative paths now use the correct directory when resuming a session created elsewhere.
- **Mermaid flowcharts** now correctly render node groups, arrow endings, self-loops and line styles.
- **Fixed** "unknown session id" errors that occurred after the leader process crashed or was killed.
- **Pasted images** now survive interjections and queue edits instead of being dropped.
- **Managed MCP connectors** (Slack, Linear, etc.) now appear correctly when using leader mode.


# 0.2.47

## Features

- **stateDiagram** Mermaid blocks now render as diagrams instead of source fallback.

## Bug Fixes

- **Pasted images** now survive interjections and queue edits instead of being dropped.
- **Managed MCP connectors** (Slack, Linear, etc.) now appear correctly when using leader mode.


# 0.2.46

## Features

- **Mermaid flowcharts** now render with fewer avoidable edge crossings.

## Bug Fixes

- **Fixed `grok --resume`** failing on empty image-only session folders left by cross-directory pastes.
- **Fixed pasted images** and relative paths using the wrong directory after cross-cwd resume.
- **Fixed Mermaid flowcharts** that silently rendered wrong diagrams for & groups, circle/cross endings and self-loops.
- **Fixed zsh tab-completion** for subcommands after the optional prompt argument was added.
- **Fixed "unknown session id" errors** after the leader process crashed or was killed.
- **Fixed repeated auto-compaction attempts** when the session is credit-blocked or auth is non-refreshable.

## Performance

- **Parallel tool calls** on the same path (multiple greps etc.) now execute concurrently.


# 0.2.45

## Features

- **Mermaid diagrams** now render to images when you click Open in a code block (on by default).

## Bug Fixes

- **Fixed** rare conversation corruption when skills changed while a tool call was still running.
- **Fixed** `grok --resume` failing on empty image-only session folders left by cross-directory pastes.
- **Fixed** pasted images and relative paths using the wrong directory after resuming a session from another folder.
- **Welcome screen logo** no longer renders as invalid characters on legacy Windows command prompts and PowerShell.
- **Fixed** "unknown session id" errors that occurred after the leader process crashed or was killed.


# 0.2.44

## Features

- **K/J** keys now snap the viewport to the top of previous or next assistant responses.
- **J/K** (vim mode) now navigate between assistant responses in scrollback.
- **sequenceDiagram** mermaid blocks now render as Unicode lifeline diagrams instead of source fallback.

## Bug Fixes

- **Interjecting** while editing a queued prompt no longer strands the composer or blocks the queue.
- **Mid-turn interjections** now appear as separate user messages instead of being appended to tool results.
- **Project MCP config** touches no longer trigger repeated reload storms.

## Performance

- **Inference requests** recover faster from silent engine stalls instead of waiting the full idle timeout.


# 0.2.43

## Bug Fixes

- **ask_user_question** tool can now be enabled in allowlists without requiring plan-mode tools.
- **Shift+Tab** mode cycling (Normal → Plan → Auto-Approve) works again in the agent view.
- **Ctrl+C** now cancels a blocking `grok update` cleanly instead of leaving an orphaned download repainting the terminal.


# 0.2.42

## Bug Fixes

- **ask_user_question** tool can now be enabled in allowlists without requiring plan-mode tools.
- **MCP servers** provided at session start now persist across config hot-reloads.


# 0.2.41

## Features

- **Compaction completion message** now shows the before → after token reduction instead of only the final count.

## Bug Fixes

- **Fixed token count after compaction** so the displayed number no longer jumps back up on the next model response.
- **Fixed plugin skill loading** when a manifest lists skill directories directly instead of a parent skills/ folder.

## Performance

- **Fixed memory context injection** on resume so the prompt prefix stays byte-stable and KV cache is preserved.


# 0.2.40

## Features

- **`grok --debug`** now produces per-session log files under ~/.grok/debug/ even with a leader process.

## Bug Fixes

- **Doom-loop warnings** now correctly describe cycles and distinct edit failures instead of claiming identical arguments.
- **Model list changes** from config or cache now appear in already-connected TUI and IDE clients without restart.


# 0.2.39

## Features

- **run_terminal_cmd** can stream live stdout/stderr chunks when the workspace flag is enabled.
- **/session-info** now displays the current turn index.
- Server-synced and bundled skills are now discovered from launcher-injected directories.
- **Background `&` operator** is now allowed by default in terminal commands.

## Bug Fixes

- **Resumed subagents** no longer loop forever during auto-compaction on large context windows.
- **Background task** descriptions and & rejection messages now correctly name the real parameters.
- **Doom-loop detection** no longer falsely triggers on distinct failing tool calls.


# 0.2.38

## Features

- **Watching status line** now appears when background monitors, loops, or subagents can wake the agent.

## Bug Fixes

- **Default model selection** now correctly chooses the intended entry when multiple models share a slug.
- Minor bug fixes


# 0.2.37

## Features

- **MCP tool result queries** now list only command-line tools actually present on your system.
- **`grok update`** now restarts any older running leader so all clients get the new binary.
- **Long-running bash commands** that hit the timeout are now moved to the background by default instead of killed.

## Bug Fixes

- **Subagents** now correctly receive web_search and x_search tools from the parent session.


# 0.2.36

## Features

- **Large MCP tool results** are now saved with the correct extension and the model receives better hints for querying them.

## Bug Fixes

- **Fixed false-positive doom-loop terminations** when many parallel tool calls fail together in one batch.
- **Fixed a crash** that could occur during auto-compaction when resuming a session containing reasoning content.


# 0.2.35


# 0.2.34

## Features

- **`grok login`** now defaults to device code flow, which works reliably in SSH, WSL, VPN, and browser-restricted environments.

## Bug Fixes

- **Fixed a hang during auth refresh.**


# 0.2.33

## Bug Fixes

- **Fixed duplicate turn output** when attaching a second client to an active leader session.
- Fixed **Send now** on queued prompts.


# 0.2.32

## Features

- **Slash commands** from project plugins now appear correctly in every open conversation after a plugin change.

## Bug Fixes

- **Prompts submitted rapidly** now stay in correct submission order in the queue.

## Performance

- **Grep searches** on large repositories are now substantially faster and no longer hit the 60-second timeout.


# 0.2.31

## Bug Fixes

- **Marketplace skills** without proper descriptions are now hidden from listings instead of flooding the model with tables.
- **Prompts submitted rapidly** now stay in correct submission order in the queue.

## Performance

- **Grep searches** on large repositories are now substantially faster and no longer hit the 60-second timeout.


# 0.2.30

## Features

- A new plugin install suggestion appears above the prompt when you type a known marketplace plugin name or domain.

## Bug Fixes

- **Trace uploads** and remote session restores now succeed with a deployment key and no browser login.
- **Resumed sessions** no longer pad the sticky prompt with empty rows; cancelling a turn now keeps the rest of the prompt queue intact.
- Cancelling a running prompt no longer leaves the interface stuck on the cancelling spinner.


# 0.2.29

## Bug Fixes

- **`/rewind`** before a compaction boundary no longer leaves later prompts in context.

## Performance

- **Resuming large sessions** is now substantially faster with no data loss.


# 0.2.28

## Bug Fixes

- **Images** read via read_file are now downscaled even when small in bytes but large in pixels.
# 0.2.27

## Features

- **Image and video generation** tools now include the saved filename and session folder in their output.

## Bug Fixes

- **Monitor output** no longer appears as raw XML in the conversation view during leader sessions.
- **Windows commands** containing `&` are no longer incorrectly rejected by `run_terminal_cmd`.
- **Python -c** save-to-file reminder now suggests correct commands on Windows.


# 0.2.26

## Bug Fixes

- **Large pasted content** no longer triggers context-window errors or breaks compaction and memory flush.
- **API-key users** can now run `grok agent --leader` without forced interactive login or timeouts.
- **Compaction** no longer retries endlessly on credit, size, or auth failures; shows a clear message instead.
- **Windows PowerShell and cmd.exe** no longer falsely reject commands containing `&`.
- **web_fetch** no longer crashes the CLI on pages whose root element matches a cleaning selector.
# 0.2.25

## Bug Fixes

- **Session titles** now generate reliably even for very long initial messages.


# 0.2.24

## Bug Fixes

- Minor bug fixes
# 0.2.23

## Features

- **Leader sessions** can now be viewed and controlled from multiple clients with a live dashboard.
- **Sessions** can now be deleted directly from the /resume history picker.

## Bug Fixes

- **MCP plugin servers** with bundled OAuth client IDs now authenticate correctly.
# 0.2.22

## Bug Fixes

- **Authentication errors** with static API keys now surface a clear error instead of hanging the turn.


# 0.2.21

## Features

- **allowed_models** in config.toml now restricts which models appear in the picker and `/model` command.

## Bug Fixes

- **Code navigation** now returns correct results for secondary project windows with different working directories.
# 0.2.20

## Bug Fixes

- **MCP servers** declared in both a plugin's.mcp.json and plugin.json are now registered instead of dropped.
- **Git operations** now correctly target the repository for each session's working directory.
# 0.2.19

## Features

- **Monitors** now appear labeled in background-task reminders after compaction and can be terminated by name.

## Bug Fixes

- **Reading images** with text-only models no longer triggers repeated 400 errors that brick the session.


# 0.2.18

## Features

- **Official xAI plugin marketplace** now appears automatically in the Marketplace tab on first launch.
- **Image and video generation** now use api.x.ai directly for all users.
- **New image-to-video and reference-to-video tools** are now available for generating videos from images.
- **New imagine skill** provides prompt-craft and workflow guidance for image generation and editing tools.

## Bug Fixes

- **image_edit** now correctly resolves pasted or attached images referenced as [Image #N].
- **Background subagent completions** are no longer reported twice when the agent is idle.
- **Subagents** now use the same model as the parent session by default.


# 0.2.17

## Features

- **Image and video generation** tools now emit structured paths so the pager renders media without regex scraping.
- **Compaction summaries** now use a more detailed structure that improves recovery after context reset.
- **image_gen** can now be enabled via the harness model using [features] in config.toml or the GROK_IMAGE_GEN_HARNESS env var.
- **Improved config refresh** on new sessions from the shell.

## Bug Fixes

- **--restore-code** no longer detaches the source repository when resuming a forked-worktree session from a different directory.
- **Read tool** string coercion bug fixes.
- **ICO images** pasted or read from disk are now automatically converted to PNG before being sent to the model.
# 0.2.16

## Features

- **New segments compaction mode** writes per-segment markdown files that the model can read to recover pre-compaction detail.
- **Claude and Cursor compatibility scanning** (skills, rules, AGENTS.md) can now be toggled individually via env vars or config.toml.
- **grok inspect** now shows the resolved on/off state and source for every Claude/Cursor compatibility toggle.
- **Cursor MCP servers and hooks** are now discovered and can be disabled independently via GROK_CURSOR_MCPS_ENABLED / GROK_CURSOR_HOOKS_ENABLED.

## Bug Fixes

- **Streaming tool output** (bash/write_file) now renders completely in the pager instead of only the latest chunk.
- **Streaming bash tool output** now appears correctly in the pager scrollback.
- **Routing a native tool** (e.g. scheduler_create) through use_tool now gives a clear corrective error instead of an unrecoverable loop.
- **"Starting session..."** spinner no longer gets stuck when zero MCP servers are configured.
- **Subagents** now use the correct harness after switching models mid-session.
- **Fixed long startup delays** when an external auth provider binary hangs or fails.
- **Subagent conversations** no longer receive unrelated monitor events or background task completions from the parent.
- **The /loop command** now accepts natural-language intervals instead of always defaulting to 10 minutes.
- **Fixed blank output** on completed bash or code-execution cards after shell restart or reconnect.

## Performance

- **Large pasted images** no longer bust the prompt cache or exceed the 50 MiB request limit.


# 0.2.15

## Features

- **Permission prompts** now remember your last choice across tools and let you configure the first-prompt default in config.toml.


# 0.2.14

## Features

- **Generated images and videos** can now be opened directly from the terminal UI via buttons or clicks.
- **Background tasks panel** now groups items, supports collapsible sections, and has clearer styling for monitors and loops.

## Bug Fixes

- **Session titles** are now generated reliably using a fixed default model.
- **--permission-mode** now correctly overrides the permission_mode setting from config.toml when launching sessions.


# 0.2.13

## Bug Fixes

- Miscellaneous bug fixes


# 0.2.12

## Features

- **Computer connection status** now shows a connecting pill during terminal session initialization.
- **/check** and subagents now read and follow full AGENTS.md rules from the repo.

## Bug Fixes

- **--max-turns** now correctly counts tool-use cycles instead of total messages.
- **@-mention file search** now works again for local agent sessions.
- **Rendered images, files, and citations** now replay correctly in chunk-mode history.
- **`/context`** now displays the correct auto-compact threshold for the active model instead of always 85%.
- **Model responses** are no longer silently dropped when the gateway emits legacy channel values.
- **Prompt responses** no longer resolve before the turn's final output chunks reach the client.


# 0.2.11

## Bug Fixes

- Minor bug fixes
# 0.2.10

## Features

- **`/check`** has been renamed to **`/check-work`**; old command continues to work during transition.

## Bug Fixes

- **Images smaller than 8×8 pixels** are now rejected with a clear message instead of producing blocky results.


# 0.2.9

## Features

- **Added --device-code** as alias for device authentication and improved headless auth error messages.


# 0.2.8

## Features

- **New /login** slash command lets you re-authenticate from within a session without quitting.
- **Compaction summaries** now include the full transcript path so the model can reference prior details.
- **Cursor skills and rules** are now discovered alongside Grok and Claude directories.

## Bug Fixes

- **Fixed monitor tool** schema to show the correct 10-hour default timeout.
- **Fixed a panic** that could occur when installing marketplace plugins.


# 0.2.7

## Features

- **Image generation and image editing** can now be toggled independently via [features] in config.toml.

## Bug Fixes

- **Background tasks** started inside subagents now continue running after the subagent session ends.


# 0.2.6

## Bug Fixes

- **Background tasks** started inside subagents now continue running after the subagent session ends.
- **Image description** now reliably uses the grok-build model instead of falling back to the active session model.


# 0.2.5

## Bug Fixes

- **Drag-and-drop** and pasting images or files now works correctly on Windows.


# 0.2.4

## Features

- **image_gen** now uses the higher-quality grok-imagine-image-quality model.

## Bug Fixes

- **read_file** now correctly passes embedded base64 images to the model as vision tokens instead of truncated fragments.


# 0.2.3

## Features

- Memory system: /remember command, note modal with raw/enhanced preview, x.ai/memory/rewrite ACP extension, Ctrl+F fullscreen toggle for /memory modal.
- Agent configuration: /config-agents modal with agents, personas, and defaults.
- Goal classifier: end-to-end goal tracking with subagent-powered classification.


# 0.2.2


# 0.2.1

## Bug Fixes

- **Pasting or dropping images** now succeeds for truncated, CRC-corrupt, or tiny files instead of failing silently.


# 0.2.0

## Performance

- **Large chat sessions** now use substantially less memory and run faster during forks, rewinds, and compaction.


# 0.1.220-alpha.4

## Features

- **Version output** now shows [alpha] or [stable] in `--version`, banners, `/session-info`, and `grok inspect`.
- **New "always-approve"** option appears first in permission dialogs. Bash commands now resolve reliably on NixOS and Homebrew installs.

## Bug Fixes

- **Auxiliary model calls** such as session titles and image descriptions now succeed for API-key users.


# 0.1.220-alpha.3

## Bug Fixes

- **Pasting or dropping files from Finder** and handling multiple images now works reliably on macOS.
- **Clipboard copy and paste** now works reliably on Linux Wayland desktops.


# 0.1.220-alpha.2

## Bug Fixes

- **Web and X search tools** now activate correctly after switching to a model that supports backend search.


# 0.1.220-alpha.1

## Features

- **Release notes** are now fetched from the CDN instead of being bundled inside the binary.

## Bug Fixes

- **--restore-code** now stashes dirty changes before restoring and shows clear success or failure messages.
- **Shell** background tasks now create their output file immediately and AwaitShell exits early on pattern match.


# 0.1.220

## Features

- **Model selection** now respects API-key permissions and defaults auxiliary calls like summaries to your current model.
- **Release notes** command now loads the latest notes from the network instead of bundled files.
- **Version output** now shows [alpha] or [stable] labels based on the release-channel pointer.
- **Permission prompts** now offer an always-approve option; shell commands resolve more reliably across installs.

## Bug Fixes

- **--restore-code** now safely stashes your uncommitted changes before restoring and shows clear success or failure messages.
- **Pasting or dropping files** from Finder now works reliably and images no longer lose their numbers or chips.
- **Background shell output** is now available immediately after starting and AwaitShell exits early when a pattern matches.
- **Copy to clipboard** now works reliably on Linux Wayland desktops using wl-copy or xclip fallbacks.
- **Web and X search** now activate correctly after switching to a backend-search model without restarting the session.


# 0.1.219-alpha.1

## Bug Fixes

- **read_file** now always returns the file content instead of a duplicate-read message after compaction.


# 0.1.219

## Bug Fixes

- **read_file** tool now always returns file content instead of a duplicate-read message after compaction.


# 0.1.218-alpha.1

## Features

- **Project instruction files** now recognize.claude/CLAUDE.md and.claude/CLAUDE.local.md.

## Bug Fixes

- **run_terminal_command** on Windows PowerShell no longer suggests unavailable Unix utilities.


# 0.1.218

## Features

- **Project instruction files** named.claude/CLAUDE.md are now discovered automatically.

## Bug Fixes

- **Windows PowerShell users** no longer see model suggestions for Unix commands that do not exist.


# 0.1.217-alpha.5

## Features

- **New `/export` slash command** and `grok export` CLI command let you save or copy conversation transcripts as Markdown.

## Bug Fixes

- **Image pasting** now correctly prefers images from the clipboard and keeps dragged screenshots visible instead of losing them.
- **Video generation** no longer forces short 5-second clips; longer durations now work as requested.


# 0.1.217-alpha.4

## Features

- **New `grok plugin` commands** let you manage plugins and marketplaces directly from the terminal.

## Bug Fixes

- **Image pasting** now correctly prefers images from the clipboard and keeps dragged screenshots visible.
- **Subagents** now correctly use bring-your-own-key credentials when the parent model specifies them.
- **MCP server connections** no longer fail with initialization errors under concurrent tool calls.
- **Signal sync** is more resilient to transient authentication hiccups.
- **Authentication recovery** after subscription changes no longer causes duplicate IdP calls.


# 0.1.217-alpha.3

## Features


## Bug Fixes

- **Image description** now works correctly when using custom models with bring-your-own-key configs.
- **Subagents** now correctly show the parent model's display name instead of the internal routing slug.


# 0.1.217-alpha.2

## Bug Fixes

- **Model picker** now correctly shows all entries including aliases when multiple models share the same slug.


# 0.1.217-alpha.1

## Features


## Bug Fixes

- **Tool calls** now appear in the output as soon as they start instead of waiting until they finish.


# 0.1.217

## Features

- **Todo list reminders** now appear at turn end when pending tasks remain after compaction.
- **Laziness detection** can now be enabled per-model to automatically nudge idle agents.
- **New `grok plugin` commands** let you manage plugins and marketplaces directly from the terminal.
- New `/export` command and `grok export` let you save or copy conversation transcripts as Markdown.

## Bug Fixes

- **Pasting images** from the clipboard or dragged screenshots now works reliably without temporary paths or duplicates.
- **Subagents** now correctly use per-model API keys instead of falling back to OAuth when both are present.
- **Model picker** now shows all models including aliases like "auto" when multiple entries share a slug.
- **Tool calls** now appear in the scrollback immediately when dispatched.
- **Cursor image and PDF handling** now processes attachments reliably without errors.
- Signal tracking for sessions is now more resilient to temporary login hiccups.
- Subscription detection now correctly respects access gates from the server.
- Image description now works reliably with custom models and bring-your-own-key configs.
- Subagents now show the correct parent model name instead of an internal routing slug.
- Authentication recovery after subscription changes no longer causes duplicate login attempts.
- Video generation no longer forces short ~5s clips when no duration is specified.
- **Fixed excessive authentication errors** that could occur during token refresh in background operations.


# 0.1.216-alpha.1

## Features

- **Grok** now reminds itself to take action in very long conversations while still obeying instructions like "just plan".


# 0.1.216

## Features

- **Grok** now reminds itself to execute plans in long conversations while strictly following instructions like "just plan".

## Bug Fixes

- **Image pasting** on Linux now works reliably when the clipboard contains images.


# 0.1.215-alpha.1

## Features

- **Default models** now use plan mode tools (enter/exit plan mode, ask user) unless overridden.

## Bug Fixes

- **Skills** now resolve relative links to bundled files like reference docs inside the skill directory.
- **Subagents** no longer cause the parent session to lose write or execute tools.


# 0.1.215

## Features

- **Default agent behavior** now uses plan mode tools for models without an explicit agent_type.

## Bug Fixes

- **Relative links** inside skill files now resolve to absolute paths automatically.
- **Subagents** no longer cause the parent session to lose access to tools.


# 0.1.214-alpha.1


# 0.1.214

## Features


## Bug Fixes

- **Image and video generation** no longer causes the model to repeat internal instructions about inline display.
- **Sessions** now resume reliably even if the original model is removed from the catalog.
- **Fixed crashes** when tool output or prompts contain multibyte characters like CJK or emoji.
- **Web search** now correctly switches between local and hosted tools when changing models.
- **Custom agent profiles** from clients are now respected for default models.


# 0.1.213-alpha.7

## Features

- **New settings pane** lets users configure appearance, themes, and behavior directly in the terminal UI.
- **New image_edit tool** supports reference-based editing using uploaded photos or data URLs.
- **Goal mode** now surfaces verification-blocked pauses with human-readable reasons and resume guidance.
- **Added `grok logout` subcommand** to sign out from the terminal without launching the TUI.
- **Backend search** is now enabled by default for web_search and x_search.
- **Added per-MCP-server `expose_image_base64` option** so raw image data remains available in tool output.
- **Improved credit-limit experience** for max-tier users with an inline scrollback card instead of a modal.
- **Automatically detects** mid-session subscription upgrades after a credit-limit error.

## Bug Fixes

- **Goal mode** now auto-pauses on repeated failures or doom loops and resumes cleanly without errors.
- **Fixed sessions** in directories with very long or CJK names that previously failed with "File name too long".
- **Fixed /btw errors** on Anthropic opus models caused by orphaned tool calls and temperature conflicts.
- **Doom-loop halts** now correctly report harness termination instead of misleading "user cancelled" messages.
- **API-key users** no longer see unavailable models such as grok-build that 404 on selection.
- **Image and video generation** now recover better from network issues and large response bodies.
- **Improved error messages** when hitting a subscription wall with an API key set, suggesting `grok logout`.
- **Removed --budget flag** from /goal and made goal verification stricter.
- **Fixed a crash** on exit when using MCP servers that require token refresh.
- **Fixed follow-up errors** after backend search and prevented unsupported tools from reaching certain models.


# 0.1.213-alpha.6

## Performance

- **Large sessions** now upload and process without long stalls or excessive memory use.


# 0.1.213-alpha.5

## Features

- **Web search and X search** can now execute server-side with full session persistence when GROK_BACKEND_SEARCH is enabled.
- **read_file** can now append configurable rule reminders for matching.cursor/rules files when enabled.
- **read_file** now extracts text from PowerPoint (.pptx) files in addition to PDF and images.

## Bug Fixes

- **run_terminal_cmd** now reports clearer "exit: killed (reason)" headers for processes terminated by timeout or cancellation.
- **Fixed model errors** when switching to non-reasoning models after previously setting reasoning effort.
- **Compaction summaries** no longer cause the model to echo the original summarization instructions after resume.


# 0.1.213-alpha.4

## Features

- **New /goal slash command** for tracking multi-step coding goals with live status and automatic pausing on Ctrl+C.
- **Goal mode** is now opt-in via config.toml or the GROK_GOAL environment variable.
- **Commands** in workspace/proxy mode now preserve environment variables, cwd, functions, and aliases across calls.
- **Background tasks** now report termination signals, reject self-killing patterns, and support unbounded timeouts.
- **Grok CLI** now runs natively on Intel Macs via x86_64 builds.

## Bug Fixes

- **Goal mode prompts** now work on all platforms and correctly place source code in your project directory.
- **Background tasks** now correctly trigger auto-wake even when a blocking wait times out.
- **Goal status bar** now shows the correct deliverable number matching the detail modal.
- **Task tool** now accepts model-emitted paths containing stray quotes or a leading tilde.
- **Subagent task output** no longer appears twice in the scrollback or leaves trailing system reminders.
- **Failed background subagents** now report errors and appear in task lists instead of silently disappearing.
- **Subscription purchases** now correctly refresh authentication so API calls succeed after the paywall lifts.

## Performance

- **MCP tool output** is now truncated earlier to reduce wasted context when models rarely follow up.


# 0.1.213-alpha.3

## Bug Fixes

- **Credit status** now updates live after topping up or changing spending limits when at your usage cap.
- **Paywall** now lifts automatically after subscribing via grok login or external purchase without needing to re-login.
- **Images from /imagine** no longer render twice in the scrollback.


# 0.1.213-alpha.2

## Bug Fixes

- **Fixed terminal UI corruption** on macOS that occurred when system malloc messages interleaved with the interface.


# 0.1.213-alpha.1

## Features

- **Marketplace plugins** from URLs can now be pinned to a specific commit SHA for tamper protection.
- **Terminal output** now shows both the start and end of long command results instead of only the tail.
- **/imagine** now works reliably after session reload and shows images directly in tool results.
- **Images and PDFs** read with the file tool now appear immediately instead of in a later message.

## Bug Fixes

- **Plugins and Marketplace** reloads now show a clear loading indicator instead of misleading per-item badges.
- **Search tool** now gives the model better guidance for finding MCP tools by server and action name.
- **Windows users** now get a more reliable shell (PowerShell preferred) that avoids path mangling.
- **Model list** no longer gets stuck on only grok-build after waking from sleep on macOS.
- **Custom xai_api_base_url** settings in config.toml now correctly route session commands for enterprise deployments.


# 0.1.213

## Features

- **/imagine** now renders generated images inline in tool results.
- **Images from read_file** now appear inline inside tool results instead of a follow-up message.
- **Goal mode** (opt-in) now tracks objectives and deliverables with live progress updates via the UpdateGoal tool.
- **New `/goal` slash command** lets you set objectives, track deliverables, pause/resume, and receive live progress updates during long tasks.
- **New Settings pane** (consolidated) for configuring appearance, themes, and general options directly from the UI.
- **Goal mode** is now opt-in via `[features] goal = true` in config.toml or the GROK_GOAL environment variable.
- **Rewind (Ctrl+Z)** now works in proxy/hub mode by routing file-state tracking through the remote workspace.
- **Paywall** now lifts automatically after subscribing via grok login or external purchase without needing to re-login.
- **Commands** in workspace/proxy mode now preserve environment variables, cwd, functions, and aliases across calls.
- **New /goal slash command** for tracking multi-step coding goals with live status and automatic pausing on Ctrl+C.
- **Grok CLI** now runs natively on Intel Macs via x86_64 builds.
- **New `image_edit` tool** supports editing images from reference photos.
- **Goal mode** now supports a "verification blocked" pause state with a user-visible reason.
- **read_file** now extracts text from PowerPoint (.pptx) files in addition to PDF and images.
- **Goal mode** verifiers now act as reviewers and subagent resumes use the latest ID.
- **New `grok logout` command** clears your cached login session directly from the terminal.
- **Web search and X search** now run server-side by default for improved results.
- **MCP image tools** can now expose raw base64 data so agents can forward images via file tools.
- **Max-tier users** now see an inline message with pay-as-you-go options when hitting credit limits.

## Bug Fixes

- **Windows shell detection** now prefers PowerShell and prevents Git Bash path mangling.
- **Fixed model list** getting stuck on only grok-build after waking from sleep on macOS by refreshing the catalog on every auth token update.
- **Fixed cross-platform prompt** text and ensured goal-mode workers write source code in the project workspace rather than temporary directories.
- **Enterprise config fix**: setting `xai_api_base_url` in config.toml now correctly routes session commands without also needing to set `cli_chat_proxy_base_url`.
- **Fixed hub disconnects** so the CLI degrades gracefully instead of panicking when the Computer Hub WebSocket drops.
- **Doom-loop terminations** now show "Agent was unable to make progress — turn ended in Xs." instead of the misleading user-cancel message.
- **Fixed terminal UI corruption** on macOS that occurred when system malloc messages interleaved with the interface.
- **Images from /imagine** no longer render twice in the scrollback.
- **Goal status bar** now shows the correct deliverable number matching the detail modal.
- **Task tool** now accepts model-emitted paths containing stray quotes or a leading tilde.
- **Subagent task output** no longer appears twice in the scrollback or leaves trailing system reminders.
- **Background tasks** now report termination signals, reject self-killing patterns, and support unbounded timeouts.
- **Failed background subagents** now report errors and appear in task lists instead of silently disappearing.
- **Subscription purchases** now correctly refresh authentication so API calls succeed after the paywall lifts.
- **run_terminal_cmd** now reports clearer "exit: killed (reason)" headers for processes terminated by timeout or cancellation.
- **Fixed model errors** when switching to non-reasoning models after previously setting reasoning effort.
- **Compaction summaries** no longer cause the model to echo the original summarization instructions after resume.
- **Fixed /btw** 400 errors on certain Anthropic models by cleaning up mid-turn tool state.
- **API key users** no longer see unavailable models like grok-build in the model list.
- **Image and video generation** now handles slow or interrupted network responses more reliably without generic decode errors.
- **Fixed error messages** when hitting a subscription wall with an API key set, now suggesting `grok logout`.
- **Fixed a crash** on exit when using MCP servers that require token refresh.
- **Reduced 400 errors** on background flush/dream operations for thinking models.
- **Fixed follow-up search errors** and ensured backend search only activates on supported models.
- **Subscription tier upgrades** are now detected automatically after a credit limit error without needing to log out.

## Performance

- **Large sessions** now upload and process without long stalls or excessive memory use.


# 0.1.212-alpha.5

## Bug Fixes

- **Fixed /btw side questions** failing with 400 errors on models that use extended thinking.


# 0.1.212-alpha.4

## Breaking Changes

- **Subagents** now always start fresh and must be fully briefed via the prompt instead of inheriting parent conversation history.

## Features

- **Memories** now avoid storing low-value tool counts and file lists while skipping near-duplicate entries via semantic similarity checks.


# 0.1.212-alpha.3

## Features

- **Added configuration options** for the auto-compaction threshold on a per-model or global basis.
- **Shows 'Logged in with API key'** indicator on the welcome screen when using an API key.

## Bug Fixes

- **Improved error messages** when calling use_tool with an invalid or built-in tool name.
- **Fixed context window** incorrectly dropping to 128k for some models, preventing mid-session cascading compactions.
- **Fixed incorrect context windows** for models referenced by multiple catalog entries with different keys.


# 0.1.212-alpha.2

## Features

- **Linkify URLs and file paths** across all TUI output
- **Improve /sessions modal** and /new /fork banners
- **Improve review plan UX** in the TUI

## Bug Fixes

- **Use multipart upload** for S3 payloads exceeding 8 MiB


# 0.1.212-alpha.1

## Features

- **web_fetch** now downloads and saves images and videos to disk (with magic-byte checks) instead of rejecting them.


# 0.1.212

## Breaking Changes

- **Subagents** no longer support fork_context; they always start fresh and must be fully briefed via the prompt.

## Features

- **Images and videos** now display directly in the scrollback with click-to-play support.
- **Skills** now show their author in the extensions list and search results.
- **web_fetch** now downloads and saves images and videos to disk instead of rejecting them.
- **Added configuration options** for the auto-compaction threshold on a per-model or global basis.
- **Shows 'Logged in with API key'** indicator on the welcome screen when using an API key.

## Bug Fixes

- **Long bash task output** no longer inflates context; full logs remain available via disk pointers.
- **Auth file locking** is now more robust against hung processes and concurrent refreshes.
- **Concurrent logins** and refreshes now wait efficiently without timeouts or retry storms.
- **Improved error messages** when calling use_tool with an invalid or built-in tool name.
- **Fixed context window** incorrectly dropping to 128k for some models, preventing mid-session cascading compactions.
- **Fixed incorrect context windows** for models referenced by multiple catalog entries with different keys.
- **Improved memory quality** by removing noisy auto-saved details and avoiding duplicate stored memories.
- **Fixed `/btw` errors** on models that use extended thinking by excluding internal reasoning from side-question requests.

## Performance

- **search_tool** now instantly finds exact tool names instead of falling back to fuzzy search.
- **search_tool** now better matches compound tool names like grafana-ai__SearchDashboards.


# 0.1.211-alpha.3

## Features

- **New browser tools** for tab and network inspection.
- **Browser tools security hardening** with cookie mode and domain allowlisting.
- **Plugin install shorthand** now accepts owner/repo format.
- **Memory modal** now renders markdown with mouse interaction and scrollbar.
- **Keyboard shortcut dialog** (Ctrl+.) redesigned for discoverability.
- **Ask-question modal UX** with double-click selection and copy support.
- **Loading spinner** added to image viewer overlay.
- **PowerShell install scripts** added for Windows users.

## Bug Fixes

- **Fixed diff line numbers** to show correct absolute positions.
- **Fixed tasks stuck** in killing state in tasks pane.
- **Privacy opt-out** now persists across token refresh.
- **Fixed infinite restart loop** during auto-update failures.
- **Gitignored files** now readable by default.
- **Fixed auth refresh storm** on expired tokens.
- **/privacy opt-out** now works for personal OIDC users.
- **Model catalog** now retries after sleep/resume.
- **Fixed terminal rendering leak** on exit.


# 0.1.211-alpha.2

## Bug Fixes

- **Login now handles corrupt authentication files** by backing them up instead of prompting repeatedly.
- **Fixed loops getting stuck** when resuming sessions.
- **Stopped duplicate notifications** for background tasks and subagents waited on directly.
- **Loop indicators now disappear** properly when subagents shut down.


# 0.1.211

## Features

- **Added browser tools** for navigation, screenshots, and network inspection via Chrome.
- **Background tasks and subagents** now automatically notify the agent when they finish.
- **Browser tools** now use safer cookie handling and domain restrictions by default.

## Bug Fixes

- **Skills** now reliably apply persona instructions to subagents via shared definitions.
- **Notifications** from background tasks now render correctly without duplication.
- **Login** now recovers from corrupted auth files instead of failing repeatedly.
- **Loop tasks** no longer get stuck when resuming previous sessions.
- **Directly waited** background tasks no longer trigger extra notifications.
- **Loop indicators** are now properly removed when subagents shut down.
- **run_terminal_cmd** tool name restored; background parameter rename preserved.
- **Model picker** now recovers automatically after sleep/resume or temporary network issues instead of staying stuck on Grok Build.
- **MCP server connections** now succeed for servers that enforce audience binding on OAuth tokens.
- **read_file** now accesses gitignored files by default (grep/list_dir/search_replace still block them unless configured).
- **--no-ask-user** flag now correctly disables the ask_user_question tool in both TUI and headless modes.


# 0.1.210-alpha.6

## Bug Fixes

- **File writes on Windows** now automatically retry if briefly locked by editors or antivirus.
- **Login process** now waits up to 10 minutes instead of 5 and shows a clearer timeout message.
- **Terminal interface** no longer gets garbled by subprocesses like git or npm.
- **Plan mode** no longer activates unexpectedly from tool descriptions containing similar phrases.
- **Authentication** now recovers immediately when using multiple terminals after token refreshes.


# 0.1.210-alpha.5


# 0.1.210-alpha.4

## Bug Fixes

- **`grok sessions search`** now finds local and remote sessions correctly, and **`grok -r <id>`** restores remote sessions reliably.


# 0.1.210-alpha.3


# 0.1.210-alpha.2

## Features

- **Tool permission prompts and lists** now clearly show server name followed by action.


# 0.1.210-alpha.1

## Bug Fixes

- **Model list** no longer collapses to default after long inactivity or re-login.
- **Session restore** no longer crashes on Windows after system reboot.
- **Fixed authentication failures** after token refresh errors.
- **Completed background tasks** now remain queryable via get_task_output.

## Performance

- **Interactive sessions** now initialize MCP servers progressively for faster startup.


# 0.1.210

## Features

- **Faster startup** for interactive sessions with progressive MCP loading.
- **MCP tools now show** as 'Server: Action' in permission screens.

## Bug Fixes

- **Full model list** now survives long inactivity, logins, and token refreshes.
- **Fixed crash** restoring sessions on Windows after system reboot.
- **Windows file saves** now retry brief editor or AV locks automatically.
- **Completed background tasks** now remain queryable via get_task_output.
- **Fixed terminal garbage** during auto-updates on macOS.
- **`grok sessions search`** now finds local and remote sessions correctly, and **`grok -r <id>`** restores remote sessions reliably.
- **Login timeout** increased to 10 minutes with clearer error message.
- **Fixed pager getting stuck in plan mode** due to tool titles mentioning plan commands.


# 0.1.209

## Features

- **Read file** now renders PDF pages as images with optional page range.
- **PDF files now render as images** when read by the AI assistant.
- **PDF files can now be read as text** by specifying pages='text' in the read tool.

## Bug Fixes

- **Subagents** now show as cancelled when you reject dangerous commands like `rm -rf`.
- **Model selection** no longer resets on login or token refresh.
- **Background tasks** now wait for completion when `block=true` is requested.
- **Subagent summaries** now show accurate tool calls and turns.
- **AI now sees** completed background bash tasks between turns.
- **Sessions with content** now appear in desktop sidebar after restart.
- **AI warns** about duplicate background tasks before starting new ones.
- **Resumed sessions now inform** the AI about any background tasks still running from before.
- **Background tasks automatically resume** the conversation when they finish, even if idle.
- **Python background scripts now stream output** in real-time without buffering delays.


# 0.1.208-alpha.2

## Features

- **PDF files** can now be read to extract and display rendered page images.
- **PDF files now render as page images** when using the read_file tool.
- **Extract plain text from PDFs** using pages="text" or format="text" in read_file.

## Bug Fixes

- **Subagents cancelled by permission prompts** now correctly reported to parent agents.
- **Custom model selections** no longer reset on login, logout, or token refresh.
- **Blocking task output requests** now wait reliably for completion.
- **Subagent stats** in task output and session cards now accurate.
- **Background task completions** now promptly visible to model between turns.
- **Saved sessions** now appear correctly in desktop sidebar after restart.
- **Resumed sessions now inform model** about previously running background tasks.
- **Model automatically wakes up** when background tasks complete.


# 0.1.208-alpha.1

## Features

- **Shell command suggestions** from history and PATH now appear as ghost text.
- **File and directory completion** now available in shell suggestions.
- **AI shell command suggestions** enabled via GROK_SUGGESTIONS_AI=1 environment variable.
- **Plan approval status** now clickable; new Abandon option exits plan mode.

## Bug Fixes

- **Fixed restoring sessions** from cloud without local cache.
- **Fixed image errors** when loading old sessions with bad images.
- **Skill commands** now display cleanly in history, titles, and search.
- **Cloud sessions** now report correct OS, shell, paths, and git status.


# 0.1.208

## Features

- **Shell command suggestions** now appear as dim italic ghost text after the cursor.
- **File and directory path completion** now available in shell suggestions.
- **AI-powered shell suggestions** configurable via `[suggestions]` or env vars.
- **Plan status** now clickable; new **Abandon plan** button and `d` key.

## Bug Fixes

- **Remote session restores** no longer fail with 401 auth errors.
- **Corrupt images** in history are now skipped or replaced with placeholders.
- **Skill commands** now display cleanly in history, search, and titles.
- **Cloud sessions** now use correct remote OS, shell, paths, and git status.
- **Bundled skills** like `/check` and `/best-of-n` now work in TUI sessions.


# 0.1.207

## Features

- **New /check-work (/verify, /check) and /best-of-n (/bon)** slash commands now work in headless mode.

## Bug Fixes

- **Fixed session workspace setup** so git, shell, and file operations no longer fail.


# 0.1.206-alpha.1

## Features

- **Simple mode** now persists across sessions in config.toml.

## Bug Fixes

- **Git and bash commands now work** reliably in the desktop app on Windows.
- **Slash command autocomplete** no longer duplicates unique skill entries.
- **Git branch changes** now update the status bar after shell commands.


# 0.1.206

## Features

- **Simple mode** now saved in config.toml across restarts.

## Bug Fixes

- **Fixed git and bash commands** failing in Grok Desktop on Windows.
- **Fixed duplicate slash commands** in autocomplete dropdown.


# 0.1.205-alpha.1

## Features

- **New `/check-work`** and **`/best-of-n`** commands** for self-verification and parallel task solving.

## Bug Fixes

- **Fixed authentication failures** during file uploads.
- **Fixed terminal display corruption** from mouse events and subprocess output.
- **Fixed sessions stuck** on read-only filesystem after login.


# 0.1.205

## Features

- **New `/check-work` and `/best-of-n`** commands for self-verification and parallel solving.

## Bug Fixes

- **Fixed authentication failures** during file and artifact uploads.
- **Fixed terminal garbling** from mouse events and child process output.


# 0.1.204-alpha.3

## Features

- **Remote tools** from hub-registered workspace servers are now discoverable and callable in sessions.
- **Worktree folders** now use readable names like 'projects-my-repo' instead of session IDs.
- **Session lists** now show AI-generated titles, git branches, repo names, and worktree labels.
- **`grok -w my-worktree`** creates a custom-named worktree folder.
- **Session picker** now groups entries by repository with headers and indents.
- **Remote workspaces** now handle multiple sessions over one connection.
- **`/usage manage`** now opens billing page; **`grok login`** enforces tiers correctly after refresh.

## Bug Fixes

- **Fixed AI-suggested paths** missing leading slash or quotes in tools like grep.
- **Fixed file upload failures** during large batch transfers over unstable connections.


# 0.1.204-alpha.2

## Bug Fixes

- **Error messages for model not found or auth failures** now show available models, auth type, version, and login fix instructions.


# 0.1.204-alpha.1

## Features

- **Subagents** now enabled by default, fixing skills like `/implement` and `/execute-plan`.

## Bug Fixes

- **Legacy web login** no longer treated as first-party xAI account for sharing and billing; prompts **`grok login`** upgrade.
- **Billing display** now includes pay-as-you-go status and monthly limits with better error handling.
- **Copy auth URLs** now works in Docker containers and displays cleanly for external providers.

## Performance

- **Large tool outputs** shortened when sent to Grok, full text saved to file in session folder.


# 0.1.204

## Breaking Changes

- **Legacy authentication** removed; run **`grok login`** to upgrade to OAuth.

## Features

- **Hub tools** like read_file now discoverable and callable in pager sessions.
- **Worktree directories** now named readably (e.g., projects-my-repo) instead of random IDs.
- **Session lists** show AI-generated titles, branches, repos, and worktree labels.
- **Session picker** now groups entries by repository for easier navigation.
- **Remote workspace servers** supported via **`--hub-workspace-mode remote=<id>`**.
- **Subagents** now enabled by default, fixing `/implement` and `/execute-plan` skills.
- **`/usage manage`** opens billing page; **`/usage`** or `show` displays credits.

## Bug Fixes

- **Legacy web login** no longer treated as premium xAI account; run **`grok login`** instead.
- **Session picker** removes duplicate blank sessions from same folder.
- **Sessions from deleted worktrees** still appear in picker after restart.
- **Credit bar** now shows pay-as-you-go status and monthly usage limits.
- **Auth URLs copy** correctly in Docker containers and display cleanly for external providers.
- **Model/auth errors** now show available models, auth type, version, and login fixes.
- **Tool path resolution** fixed for absolute paths missing leading slash.
- **Fixed upload failures** from HTTP/2 connection degradation.
- **Memory search** now reliably includes global and workspace MEMORY.md files.

## Performance

- **Large tool results** shortened in chat; full text saved to file with path link.


# 0.1.203-alpha.2

## Bug Fixes

- **`/btw` slash command** now works correctly with all backends including Anthropic.


# 0.1.203-alpha.1

## Features

- **New `grok-build-v2` agent** available in config.toml for prompt/tool testing.

## Bug Fixes

- **Fixed duplicate MCP and skill reminders** when resuming sessions.
- **Plan review overlay** now always shown, even in always-approve mode.
- **Fixed empty model list** and unknown model after login or logout.
- **Fixed unwanted login prompts** during brief network issues at startup.
- **Fixed `grok sessions list`** auth failures.


# 0.1.203

## Features

- **New `grok-build-v2` agent** option in config.toml for testing prompt and tool changes.
- **Session picker `/load`** deep search now shows spinner, content matches, and previews on expand.

## Bug Fixes

- **Resumed sessions** no longer duplicate MCP server and skill reminders.
- **Plan mode exits** now always show interactive approval, even in always-approve mode.
- **Model picker** now updates correctly after login/logout without empty lists or 'unknown'.
- **`grok sessions list`** now works reliably after login.
- **`/btw` slash command** now supports all backends like Anthropic Messages API.


# 0.1.202-alpha.1

## Features

- **Added /usage command** to view account usage and billing information.
- **Integrated Computer Hub client** for remote computer-use sessions.
- **Added --hub-url flag** to configure Computer Hub endpoint.
- **Implemented /rewind** with inline overlay, message picker, and live dimming preview.
- **Added /memory slash command** with browse and toggle routing.
- **Memory save notifications** in scrollback with remember/forget/recall instructions.
- **Added grok memory clear** CLI command with scope selection and confirmation.
- **Added /memory on|off** to toggle memory mid-session.
- **Vision support** for base64 images extracted from user queries.
- **Added --allow/--deny CLI flags** with PolicyDeny variant and deny rule UX improvements.
- **Configurable parallel verifiers** with improved verification prompt.
- **MCP connection progress UI** with system reminders and progressive strategy fix.

## Bug Fixes

- **Fixed session resume** defaulting to legacy auth instead of OIDC.
- **Reject WebLogin tokens** and force interactive login; fixed devbox auth.
- **Normalized skill names** by converting underscores to hyphens.
- **Fixed auth revalidation** to always check token validity.
- **Fixed memory search scoring**, modal UX, and pager display.


# 0.1.202

## Features

- **Add /usage command** and billing extension fix
- **Computer Hub integration** into xai-grok-shell
- **--hub-url flag** and thread hub config to shell
- **Implement /rewind** with inline overlay, picker, and live dimming preview
- **Stream tool-call argument deltas** via the buffered path
- **LocalRegistry cloneable handle** with shared dispatch across workspace
- **Add /memory slash command** with browse and toggle routing
- **Memory save notifications** in scrollback with remember, forget, and recall instructions
- **grok memory clear command** with scope and confirmation
- **Support /memory on|off** to toggle memory mid-session
- **Extract base64 images** from user queries as vision tokens
- **--allow/--deny CLI flags** with PolicyDeny variant and deny rule UX improvements
- **Configurable parallel verifiers** with improved prompt
- **MCP connection progress UI** with system reminders and progressive strategy fix
- **require_plan_approval config flag** in pager
- **Best-of-N mode** for grok-build
- **Anthropic Messages backend** with streaming reasoning traces end-to-end

## Bug Fixes

- **Fix terminal tool calls** when agent runs via scheduled task on Windows
- **Fix resume** defaulting to legacy auth instead of OIDC
- **Reject WebLogin tokens** and force interactive login; fix devbox auth
- **Normalize underscores to hyphens** in skill names
- **Fix auth** to always revalidate
- **Fix memory search scoring**, modal UX, and pager display


# 0.1.201-alpha.1

## Features

- **New `--self-verify` flag** starts automatic work-checking after each response.

## Bug Fixes

- **Reasoning effort settings** no longer incorrectly apply to unsupported models like grok-build.
- **Web search, image gen, and video gen tools** now avoid 401 errors during auth provider gaps.
- **`/share` command** now works in token's final 5 minutes and guides API key users to login.
- **`/billing` command** now works reliably in token's final 5 minutes.
- **Deny rules in config** now block actions even with `--always-approve`.
- **Skills** now provide follow-up context automatically after use.


# 0.1.201

## Features

- **New `--self-verify` flag** starts sessions with automatic work-checking after each response.

## Bug Fixes

- **Reasoning effort settings** no longer incorrectly apply to models without support, like grok-build.
- **Web search, image generation, and video generation tools** no longer fail with missing API key errors.
- **Deny rules in config.toml** now block tool calls even with auto-approval enabled.


# 0.1.200-alpha.1

## Features

- **grok-build model** now defaults to Chat Completions API.

## Bug Fixes

- **MCP tools** now work more reliably by always fetching schemas before use.
- **MCP tool calls** preserve all custom arguments without dropping extras.


# 0.1.200

## Bug Fixes

- **MCP tools** now work more reliably by enforcing schema lookup before calls.


# 0.1.197-alpha.5

## Bug Fixes

- **MCP tools** now always use correct parameters by requiring schema lookup first.


# 0.1.197-alpha.4

## Features

- **`/model` command** now lets you pick reasoning effort levels like high/medium/low for supported models.
- **Images in tool output** now appear as pictures the AI can see, not long base64 strings.

## Bug Fixes

- **Shell commands now work** in Grok Desktop on Windows.
- **Image compression messages** now explain if size, dimensions, or both triggered resizing.


# 0.1.197-alpha.1

## Features

- **Agent profiles** now support tool allow/deny lists, reasoning effort, turn limits, and reliable model overrides.
- **Custom models** in config.toml now support both Bearer token and x-api-key authentication.
- **Built-in docx, xlsx, pptx skills** now available for office document editing.
- **MCP tools** now time out after 100 minutes by default instead of 1 minute.
- **MCP server reminders** now include instructions to discover and use tools correctly.
- **New Verify mode** (Shift+Tab) runs automatic checks and fixes after each agent turn.
- **Tool call arguments** now stream progressively as generated by the model.

## Bug Fixes

- **Agents auto-exit plan mode** without prompts when started in always-approve mode.


# 0.1.196-alpha.6

## Bug Fixes

- **Fixed interactive shell, command syntax, and git editor issues** on Windows.


# 0.1.196-alpha.2

## Features

- **New `grok-build-orchestrator` agent** uses a lead model to coordinate subagents for builds.

## Bug Fixes

- **Custom UI settings in config.toml** no longer get lost when saving other changes.


# 0.1.196

## Features

- **New `grok import`** resumes Claude Code sessions with `grok import <id/path>`.
- **New `grok-build-orchestrator`** coordinates subagents for complex builds.
- **`/skills` panel** now includes **toggles** to enable or disable individual skills.

## Bug Fixes

- **Discovers CLI tools** from `.zshrc`/`.bashrc`/virtualenvs (e.g., tclips).
- **Custom UI settings** like timestamps persist across config saves.
- **Model switches** rebuild harness and hold prompts until complete.
- **Model list refreshes** correctly after login or logout.
- **Subagents** now execute tools in the **specified working directory** instead of always using the parent's.
- **Generated session titles** now display correctly in `/sessions` list and terminal.
- **Fixed shell detection on Windows** for interactive PTYs, command syntax, and git editors.
- **Fixed `grok share`** failing with backend ZodError due to duplicate request headers.

## Performance

- **Faster session startup** on large repositories by backgrounding project info.


# 0.1.195-alpha.7

## Features

- **New `grok login --devbox`** option for signing in from headless devbox environments without browser access.
- **Skill file reads** now display as **'Skill {name}'** for cleaner scrollback view.


# 0.1.195-alpha.4

## Features

- **Billing credits and usage** now fetchable for display in pager/desktop.
- **New `grok mcp doctor`** diagnoses MCP servers and suggests fixes.
- **Session picker** now shows precise last activity times.

## Bug Fixes

- **Fixed generation hangs** on persistent authentication failures.


# 0.1.195-alpha.3

## Bug Fixes

- **Image and video generation** now works reliably after long idle periods.
- **Fixed crashes** from large images read by the Read tool.
- **Improved Windows support** for auto-updates, installation, and shell commands.


# 0.1.195-alpha.2


# 0.1.195-alpha.1

## Bug Fixes

- **Terminal stays responsive** when cancelling shell commands.
- **Fixed requests** to Anthropic models from remote model lists.


# 0.1.195

## Features

- **New `grok mcp doctor`** diagnoses MCP server configuration and connectivity.
- **Subagents** can now target specific directories without new worktrees.
- **Session picker** now shows accurate last activity times.

## Bug Fixes

- **Image and video generation** now works reliably after long idle periods.
- **Large images read from files** no longer crash API requests.
- **Terminal stays responsive** when cancelling long-running shell commands.
- **Anthropic models** from remote lists now route to correct API endpoints.
- **Fixed resume and fork crashes** from old session files.
- **Fixed crashes from large images** in tools and attachments.
- **Fixed generation hangs** on persistent authentication failures.


# 0.1.194-alpha.1

## Features

- **After plan approval**, Grok suggests `/implement` for structured coding when available.

## Bug Fixes

- **Model lists and pickers** now show only models accessible via your `GROK_CODE_XAI_API_KEY`.


# 0.1.194

## Features

- **Plan mode** now recommends the **/implement** skill after user approval when available.

## Bug Fixes

- **`grok models`** and TUI picker now show team-specific models when **`GROK_CODE_XAI_API_KEY`** is set.


# 0.1.193-alpha.4


# 0.1.193-alpha.3

## Features

- **Pressing Ctrl+C** before any response now restores your prompt to the input box, ready to edit or resend.
- **Native Windows x86_64 support** now available via install script and auto-updater.

## Bug Fixes

- **Configured MCP tools** now appear reliably in initial session context messages.


# 0.1.193-alpha.2

## Bug Fixes

- **Fixed false-positive loop warnings** for different shell commands during edit-test cycles.


# 0.1.193-alpha.1

## Features

- **Native Windows support** now available experimentally for running Grok without WSL.
- **Interactive menu for importing Claude settings** lets you select items like permissions or MCP servers on welcome screen or `/import-claude`.

## Bug Fixes

- **Image compression** now skips small files and warns if large images can't be resized under 3.75 MB limit.
- **Feedback errors** now appear in the session instead of failing silently.
- **Clearer login error messages** now suggest `grok login`, API key env var, or config.toml.


# 0.1.193

## Features

- **Native Windows support** (opt-in) lets you run Grok CLI directly without WSL, with MCP server compatibility.
- **New interactive import** for Claude settings (Ctrl-I or /import-claude) lets you pick permissions, env vars, MCP servers, hooks, or paths to migrate.
- **Cancel with Ctrl+C** before any response to restore your unsent prompt (text and images) to the input box.
- **dontAsk permission mode** now auto-rejects tools without prompting in headless sessions.

## Bug Fixes

- **Large images** (>3.75MB after decoding) now compress to JPEG efficiently, keeping originals if needed.
- **Authentication errors** now clearly suggest `grok login`, env var, or config.toml api_key.
- **Fixed false-positive loop warnings** for different shell commands during edit-test cycles.
- **Configured MCP servers** now appear reliably in the initial session prompt.


# 0.1.192-alpha.3

## Features

- **Import settings** from `.claude/` (permissions, env vars, MCP servers) into `config.toml` using startup prompt or `/import-claude`.
- **Claude migration** now fully disables legacy `.claude/` fallbacks after import.

## Bug Fixes

- **Worktree errors** now display the specific failure reason instead of generic messages.
- **Shell commands** now appear immediately in the status bar during execution.


# 0.1.192-alpha.2

## Breaking Changes

- **Temporarily removed the `Auto` option** from the `/model` picker.


# 0.1.192-alpha.1

## Features

- **Skills reload automatically** when editing and saving SKILL.md files without restarting.

## Bug Fixes

- **Slash commands like `/loop`** now only appear when supported by the current model.
- **Fixed empty model hash** display in status bar and feedback channels.
- **Fixed authentication errors** for OIDC users with per-model API keys in config.toml.


# 0.1.192

## Breaking Changes

- **Temporarily removed the `Auto` option** from the `/model` picker.

## Features

- **Import Claude settings** from `.claude/` into `.grok/config.toml` via welcome screen ('i') or `/import-claude`.
- **Skills reload automatically** when editing/saving SKILL.md files without restarting `grok`.

## Bug Fixes

- **Slash commands like `/loop`** now only appear when the current model supports them.


# 0.1.191-alpha.6

## Features

- **Skills reload automatically** when you edit and save SKILL.md files without restarting.

## Bug Fixes

- **Slash commands like `/loop`** now only appear when supported by the current model.


# 0.1.191-alpha.5


# 0.1.191-alpha.4


# 0.1.191-alpha.3

## Bug Fixes

- **Ctrl+C clears drafts** without cancelling turns; press twice on empty prompt to cancel.


# 0.1.191-alpha.2


# 0.1.191-alpha.1

## Bug Fixes

- **Context bar** now reflects live token usage and matches **`/context`**.


# 0.1.191

## Features

- **Improved `grok login`** with automatic code delivery from browser consent page.

## Bug Fixes

- **Context bar** now accurately reflects live token usage matching `/context`.
- **Ctrl+C clears drafts** without cancelling turns; press twice on empty prompt to cancel.


# 0.1.190-alpha.3

## Features

- **New `[[version_overrides]]`** sections apply config patches for specific CLI version ranges.

## Bug Fixes

- **`/context`** now updates correctly with token usage per conversation turn.


# 0.1.190-alpha.2


# 0.1.190-alpha.1

## Features

- **New `cli.minimum_version`** config enforces version floor and prompts updates.


# 0.1.190


# 0.1.189-alpha.4


# 0.1.189-alpha.3


# 0.1.189-alpha.2

## Features

- **New slash command** `/feedback` available in headless and stdio agent modes.
- **New `error_reporting` config** enables Sentry independently of telemetry.

## Bug Fixes

- **Fixed session cleanup race** preventing errors in concurrent headless sessions.


# 0.1.189-alpha.1


# 0.1.189

## Bug Fixes

- **Auth screen** now has consistent styling and better spacing.
- **Fixed authentication errors** when starting new sessions after long idle.
- **Prevents auth errors** on welcome screen from background tasks during idle.


# 0.1.188-alpha.1

## Bug Fixes

- **Fixed misleading auth errors** for OIDC sessions with per-model API keys in config.toml.


# 0.1.188

## Bug Fixes

- **Fixed misleading 'Invalid API key' errors** for OIDC sessions with per-model credentials.


# 0.1.187

## Features

- **Added reset button** for tool permissions in desktop agent settings.
- **Added mid-turn interjection** via Ctrl+Enter without canceling the turn.

## Bug Fixes

- **Fixed OIDC session refresh** when using per-model API keys in config.toml.
- **Fixed write denials** on symlinked paths like `/tmp` after prior reads.


# 0.1.186-alpha.1

## Performance

- **Reduced CPU spikes** during git operations like pull and checkout.


# 0.1.186

## Performance

- **Reduced CPU spikes** from file watcher during git pull, checkout, and other operations.


# 0.1.185-alpha.2


# 0.1.185-alpha1


# 0.1.185


# 0.1.184-alpha.2


# 0.1.184-alpha.1

## Bug Fixes

- **Large images** now compressed client-side before API with user notification.
- **Plan mode reentry** reminder now includes explicit exit instructions to prevent hangs.
- **Model unavailability** on account changes now handled gracefully with auto-switch.
- **Session picker** shows all sessions; remote fetches now have timeouts.
- **Credential prompts** no longer corrupt TUI by detaching child processes from terminal.
- **Remote sessions** now matched by repo URL across machines and protocols.
- **Fixed ghost text in input field** from config warnings and log messages.


# 0.1.184

## Bug Fixes

- **Large images** now compressed client-side before API with user notification.
- **Model unavailability** after account switch now handled with clear notifications.
- **Session picker** now shows all sessions; remote loads have timeouts.
- **Credential prompts** no longer corrupt TUI in terminal commands.
- **Remote sessions** now matched by normalized repo URL across machines.
- **TUI input field** no longer shows ghost text from config warnings and log messages.


# 0.1.183-alpha.1

## Features

- **New `grok trace`** command uploads or exports session data for debugging.
- **Slash command arguments** now support fuzzy autocompletion with Tab.


# 0.1.183

## Features

- **New `grok trace`** command exports or uploads session data for debugging.


# 0.1.182-alpha.4

## Bug Fixes

- **Model list** refreshes correctly after **account or team switch** to prevent auth errors.
- **MCP tools** no longer corrupt **TUI display** with credential prompts like GPG pinentry.


# 0.1.182-alpha.3

## Features

- **`grok models`** now lists available models and the default.
- **Permission prompts** improved with numeric shortcuts and scope selector.

## Bug Fixes

- **Same-file edits** now serialize to prevent lost concurrent changes.


# 0.1.182-alpha.2


# 0.1.182-alpha.1


# 0.1.182

## Features

- **`grok models`** command now lists available models and authentication status.
- **Permission prompts** now use numeric shortcuts, scope selector, and syntax highlighting.

## Bug Fixes

- **Improved auth token refresh** prevents 401 failures in long conversations.
- **Login checks** now use shared client with 5s timeout to prevent hangs.
- **Concurrent edits** to the same file now serialize to prevent lost changes.
- **Fixed TUI corruption** from credential prompts in MCP tools like GPG pinentry.


# 0.1.181-alpha.2

## Bug Fixes

- **Improved auth token refresh** prevents failures during long conversations.
- **Added timeout** to login checks prevents indefinite hangs.


# 0.1.181-alpha.1

## Features

- **X Premium and Premium+** tiers now grant access to Grok Build.

## Bug Fixes

- **Model picker** now shows config alias names instead of internal slugs.


# 0.1.181

## Bug Fixes

- **Model picker** now displays config key names like `grok-build` for aliased models.


# 0.1.180-alpha.3


# 0.1.180-alpha.2


# 0.1.180-alpha.1

## Features

- **New `--restore-code` flag** restores original git commit on session resume.
- **Session resume** warns if current git HEAD **diverged** from original commit.
- **Welcome screen** shows **SuperGrok upsell** without active subscription.
- **`/share`** slash command hidden when sharing disabled remotely.

## Bug Fixes

- **`/privacy opt-out`** now works for OAuth2 and CLI team tokens.
- **Esc** unfocuses prompt; double-Esc cancels running turns.


# 0.1.180

## Features

- **New `--restore-code`** flag restores session commit when resuming with `-w`.
- **Session resume** now warns if **codebase HEAD** diverged from original.
- **Welcome screen** restricts actions and upsells SuperGrok without subscription.
- **`/share` slash command** now hidden when sharing is disabled remotely.

## Bug Fixes

- **`/privacy opt-out`** now works for **OAuth2 and CLI team tokens**.
- **Esc unfocuses prompt** during turns; **double-Esc from scrollback** cancels running turns.
- **Safety and policy errors** no longer trigger unnecessary re-authentication.
- **`list_dir`** cleans display paths for `.`, empty, and `./` targets.


# 0.1.179-alpha.5

## Bug Fixes

- **Prevents disk accumulation** from orphaned upload files and scratch directories in long sessions.


# 0.1.179-alpha.4

## Bug Fixes

- **Prevents disk accumulation** from orphaned upload files and scratch directories in long sessions.


# 0.1.179-alpha.3

## Bug Fixes

- **Fixed conversation retries** on empty model responses lacking content.
- **Improved bash commands** in permission prompts with syntax highlighting, wrapping, and allow-always persistence.


# 0.1.179-alpha.2


# 0.1.179-alpha.1


# 0.1.179

## Features

- **How-to guides** bundled in binary, accessible via Ctrl+P and model-readable.
- **`/logout`** slash command clears credentials and returns to login.
- **`/privacy`** slash command toggles coding data retention opt-in/out.
- **SuperGrok upsell** shown on welcome screen without subscription.
- **Live countdown** and direct cancel added for `/loop` scheduled tasks.


# 0.1.178-alpha.1

## Bug Fixes

- **Fixed authentication failures** and web search disabling during token refresh periods.


# 0.1.178

## Bug Fixes

- **Fixed authentication failures** during token refresh periods preventing 401 errors.


# 0.1.177-alpha.1

## Bug Fixes

- **New file creations** now render as all-green diffs with 'Creating' prefix.


# 0.1.177

## Bug Fixes

- **write_file tool outputs** now render as proper diffs with 'Creating' prefix.
- **Authentication token refreshes** now robust across shell, pager, and desktop.


# 0.1.176-alpha.1

## Features

- **`grok sessions search`** now finds sessions by content with scores and snippets.
- **Session picker** now combines fuzzy matches with deep content search.

## Bug Fixes

- **Scheduled tasks** now require explicit user confirmation before cancellation.
- **Ctrl+V image paste** now handles empty clipboard text and bracketed paste.


# 0.1.176

## Features

- **`grok sessions search`** now supports **full-text queries** across chat content and tools.
- **Session picker** now combines fuzzy matching with **deep content search** results.
- **Unified logs** now saved to `~/.grok/logs/unified.jsonl` with **Download Logs** in desktop.

## Bug Fixes

- **Scheduled tasks** now require **user confirmation** before cancellation.
- **Image pasting** now handles **Ctrl+V** and whitespace-only clipboard events.


# 0.1.175-alpha.1

## Features

- **Configurable announcements and tips** now load from local config files.
- **Live MCP server toggle** enables enable/disable without session restart.
- **New slash command** `/session-info` displays model, context usage, turns, and session title.

## Bug Fixes

- **Auto-update restart** now launches the correct new version.
- **Auth.json hot-reload** fixed for grok login in other terminals.
- **Claude permission settings** now merge rules across all files with correct precedence.

## Performance

- **Status bar git info** updates faster without synchronous spawns.


# 0.1.175

## Features

- **New slash command** `/dream` triggers **manual memory consolidation**.

## Bug Fixes

- **Context bar** now updates **token count** immediately after **compaction**.
- **Remote session restore** with `-w` now targets **worktree** without source pollution.


# 0.1.174-alpha.2

## Features

- **Configurable announcements and tips** now load from local config files.
- **Live MCP server toggle** enables/disables without session restart.

## Bug Fixes

- **Auto-update restart** now launches the correct new version.
- **Auth.json hot-reload** fixed for `grok login` in other terminals.

## Performance

- **Status bar git info** updates instantly without synchronous spawns.


# 0.1.174-alpha.1

## Features

- **Announcements and tips** now load from local config files.
- **MCP servers** can now be toggled live without restarting the session.

## Bug Fixes

- **Auto-update restart** now launches the correct new version.
- **Auth.json hot-reload** now detects `grok login` from other terminals.
- **API error messages** no longer include request body for privacy.
- **Status bar counting fixed**, **diff indentation preserved**, and **bash timeout** increased to 10 hours.
- **Rate limit errors** now show friendly messages with **upgrade instructions**.
- **External auth tokens** now refresh correctly on 401 errors.

## Performance

- **Status bar git info** now updates without synchronous git spawns.


# 0.1.174

## Features

- **New slash command** `/session-info` displays model, context usage, turns, and session title.


# 0.1.173-alpha.2

## Features

- **Configurable announcements and tips** now load from local config files.
- **Live MCP server toggle** enables/disables without session restart.

## Bug Fixes

- **Auto-update restart** now launches the correct new version.
- **Auth.json hot-reload** fixed for `grok login` in other terminals.
- **Fixed status bar counting**, **diff indentation**, and **increased bash timeout** to 10 hours.
- **Rate limit errors** now show friendly messages with **upgrade instructions**.

## Performance

- **Status bar git info** updates faster without synchronous spawns.


# 0.1.173-alpha.1

## Features

- **Announcements and tips** now configurable from local files like config.toml.
- **Live MCP server toggle** enables disable/re-enable without session restart.
- **New config options** `[features] managed_config` and `[endpoints] managed_config_url` for enterprises.
- **Crash handler** opt-in via `[diagnostics] crash_handler`.
- **`/loop` slash command** and **monitor tool** for recurring tasks.
- **Vim-style quit commands** `:q`, `:q!`, `:wq`, `:wq!` now exit TUI.

## Bug Fixes

- **Auto-update restart** now launches correct new version via symlink.
- **External `grok login`** now hot-reloads auth without app restart.
- **Fixed hangs** from broken streaming responses.
- **Stale background tasks** now marked complete on session reload.
- **MCP servers** loaded from `~/.claude.json` and `.mcp.json` files.

## Performance

- **Status bar git info** renders faster using cached notifications.


# 0.1.173

## Features

- **Live MCP server toggle** enables disable/re-enable without session restart.
- **Blocked state** now displayed in settings with profile images and manage button.
- **New config flags** `managed_config` and `managed_config_url` for enterprise setups.
- **MCP servers** now load from ~/.claude.json and.mcp.json files.
- **Opt-in crash handler** captures SIGSEGV/SIGBUS dumps via config or env var.
- **New /loop slash command** schedules recurring tasks with monitor tool.
- **Vim quit commands** :q, :q!, :wq, :wq! now exit TUI.
- **Signal-safe crash handler** wired into pager with opt-in config gate.

## Bug Fixes

- **Auth refresh** now handles re-auth, OIDC principal context, and stale tokens.
- **Stale background tasks** now marked complete on session reload.


# 0.1.172-alpha.1

## Features

- **Configurable announcements and tips** now load from local config files.

## Bug Fixes

- **Auto-updates** now restart with the correct new version.
- **Auth.json hot-reload** fixed for `grok login` in other terminals.

## Performance

- **Status bar** git info updates faster without synchronous spawns.


# 0.1.172

## Features

- **Announcements and tips** now configurable from local files like config.toml.

## Bug Fixes

- **Auto-updates** now restart with the correct new version displayed.
- **External `grok login`** now hot-reloads auth without restarting.

## Performance

- **Status bar git info** renders faster using cached notifications.


# 0.1.171-alpha.5

## Bug Fixes

- **Auto-update restart** now launches the correct new version and displays updated version number.


# 0.1.171-alpha.4

## Bug Fixes

- **Exit plan mode** now shows full approve/revise/feedback options instead of simple y/n prompt.


# 0.1.171-alpha.3

## Features

- **New `grok ssh`** command enables clipboard copy from remote sessions in Apple Terminal.


# 0.1.171-alpha.2


# 0.1.171-alpha.1

## Features

- **MCP servers** can now be enabled or disabled from the UI.


# 0.1.170-alpha.2

## Features

- **`grok login --device-auth`** enables login from SSH, Docker, and other headless environments without port forwarding.
- **Skills tab** added to the hooks/plugins modal.
- **Permission rules** now support path globs like `Edit(src/**/*.rs)` and recursive `**` patterns.
- **Slash command dropdown** widened to show plugin names.
- **`Ctrl+P` command palette** for quick access to commands and actions.
- **MCP integration tools** are now discoverable and usable via `search_tool` and `use_tool`.
- **Terminal multiplexer clipboard support** with automatic routing and diagnostics for tmux/zellij/screen.
- **`/btw` side-questions** now appear as a compact inline panel above the prompt instead of a fullscreen overlay.
- **Command palette sections** group commands by category for easier navigation.
- **'New Session in Worktree'** command added to the palette.
- **Environment variables** from `.claude/settings.json` are now loaded into sessions.
- **`Ctrl+Shift+V` / `Cmd+Shift+V`** pastes text inline without triggering tool calls.
- **Marketplace** now supports GitHub source type and a known_marketplaces.json registry.

## Bug Fixes

- **Login with cached credentials** no longer fails when upgrading between auth methods.
- **`Shift+1` in VSCode terminal** now correctly triggers bash mode.
- **Single-line command display** no longer shows extra newlines.

## Performance

- **Streaming zstd compression** for file uploads reduces transfer size.


# 0.1.170-alpha.1


# 0.1.170

## Features

- **`grok login --device-auth`** enables login from SSH, Docker, and other headless environments without port forwarding.
- **Skills tab** added to the hooks/plugins modal.
- **Permission rules** now support path globs like `Edit(src/**/*.rs)` and recursive `**` patterns.
- **Slash command dropdown** widened to show plugin names.
- **`Ctrl+P` command palette** for quick access to commands and actions.
- **MCP integration tools** are now discoverable and usable via `search_tool` and `use_tool`.
- **Terminal multiplexer clipboard support** with automatic routing and diagnostics for tmux/zellij/screen.
- **`/btw` side-questions** now appear as a compact inline panel above the prompt instead of a fullscreen overlay.
- **Command palette sections** group commands by category for easier navigation.
- **'New Session in Worktree'** command added to the palette.
- **Environment variables** from `.claude/settings.json` are now loaded into sessions.
- **`Ctrl+Shift+V` / `Cmd+Shift+V`** pastes text inline without triggering tool calls.
- **Marketplace** now supports GitHub source type and a known_marketplaces.json registry.

## Bug Fixes

- **Login with cached credentials** no longer fails when upgrading between auth methods.
- **`Shift+1` in VSCode terminal** now correctly triggers bash mode.
- **Single-line command display** no longer shows extra newlines.

## Performance

- **Streaming zstd compression** for file uploads reduces transfer size.


# 0.1.169-alpha.2

## Features

- **Added** `grok login --device-auth` **flag** for headless environments like SSH and Docker.


# 0.1.169


# 0.1.168-alpha.1

## Features

- **`/compact-mode`** slash command toggles denser UI layout.


# 0.1.168

## Features

- **`/compact-mode`** slash command toggles denser UI layout.


# 0.1.167-alpha.2

# 0.1.167-alpha.1

## Bug Fixes

- **Automatic cleanup** deletes stale session files older than 30 days.
- **Linux binaries** now fully static for older distro compatibility.

# 0.1.167

# 0.1.162

## Features

- **Marketplace plugin provenance** tracked in InstalledRepo with source metadata and name collision warnings.
- **web_fetch tool** enables web content fetching with domain permissions and proxy support.
- **video_gen tool** integrated with remote settings gating and session-based authentication.
- **/feedback command** activates dedicated mode for submitting session feedback to x.ai.
- **Git stash-all** via ACP extension and desktop Changes panel button including untracked files.
- **Pager --oauth flag** forces OAuth flow for deferred welcome-screen authentication.
- **Fractional bash timeouts** like 30.5s now supported in toolset.bash config.toml.
- **.mcp.json support** enables team-shared MCP servers via project root config with full precedence.
- **Auth info extension** returns login method ID and email via x.ai/auth/info ACP method.
- **Branch diff section** shows PR-style diffs vs default branch in desktop changes panel.
- **Jujutsu VCS support** enables detection, operations, routing, and workspaces.
- **Startup announcements** emits x.ai/announcements/refreshed ACP notification on agent init.
- **Auto-enroll updates** defaults to enabling automatic updates without interactive prompt.
- **Subagent visualizations** in pager now show persona, role, and model metadata.
- **Bundled agents** discovered from ~/.grok/bundled/agents/ with lowest precedence after project/user/built-in.
- **Pager welcome screen** displays rotated tip-of-the-day from RemoteSettings honoring config and env overrides.
- **image_gen/video_gen tools** instruct model to display generated media inline via markdown.
- **Project plugins** default disabled with enable via [plugins].enabled and added counts for UI.
- **Enterprise managed configs** via ~/.grok/managed_config.toml/requirements.toml with `grok inspect` and `grok setup`.
- **web_fetch tool** enabled via [features] web_fetch in config.toml alongside env/remote settings.
- **grok login defaults to OAuth** instead of legacy relay; use --legacy or GROK_OAUTH_ENABLED=0 for old behavior.
- **Clipboard copy over SSH/tmux** now reaches local terminal via OSC 52 alongside native clipboard writes.

## Bug Fixes

- **Cross-CWD session resume** now finds local sessions under any stored directory before remote restore.
- **Automatic cleanup** removes old grok/grok-pager binaries post-update, keeping current + previous.
- **Preserves legacy credentials** when clearing OAuth scope in auth.json during login.
- **Plan file path fix** includes session-specific path in EnterPlanMode output for correct writes.
- **Rewind truncation fix** skips synthetic system-reminder messages when counting user prompts.
- **User prompt history** excludes subagent prompts preventing Ctrl+R pollution.
- **Path-not-found hints** from remote settings now reach TUI and shell agent config.
- **Legacy WebLogin users** regain managed MCP config fetching post-OIDC migration.
- **Skill discovery** handles missing frontmatter using dir name and normalizes spaced names to hyphens.
- **web_fetch PDFs** saved to disk with read_file guidance instead of garbled text.

## Performance

- **Parallel tool dispatch** accelerates multi-tool batches via concurrent execution.
- **Faster session memory staleness** via 7-day half-life default and equalized session source weights.


# 0.1.161-alpha.3

## Features

- **Interactive plan approval dialog** enables approve/reject/feedback on exit_plan_mode before building.

## Bug Fixes

- **Prevents model crashes** on large files by capping hashline_read output at MAX_LINES_READ like read_file.
- **Automatic cleanup** removes old grok/grok-pager binaries post-update, keeping only current and previous.


# 0.1.161-alpha.2

## Features

- **Parallel tool dispatch by default** accelerates multi-tool execution via batched skills and timing fixes.
- **Web fetch tool** enables controlled web content retrieval with interactive domain permissions and config gating.

## Bug Fixes

- **Fixed pager login screen** for enterprise OIDC via centralized interactive auth detection.
- **Per-key error streak detection** prevents cross-tool interference in doom loop termination.


# 0.1.161-alpha.1

## Features

- **Force OAuth login** from pager welcome screen using new `--oauth` startup flag.


# 0.1.160-alpha.8

## Features

- **Per-hook enable/disable toggles** via ~/.grok/disabled-hooks file and 'e' key with **multi-line j/k navigation fix**.
- **Bash-mode execute blocks auto-expand** after completion to display output immediately.
- **Video generation tool** supports xAI API with async polling, download and sequential MP4 naming.
- **Marketplace plugin installs** route through git_install with provenance tracking.
- **Parallel tool dispatch** via GROK_PARALLEL_TOOL_DISPATCH=1 runs concurrent Phase 2 execution.
- **VideoGenConfig threading** gates video_gen tool via feature flag and session auth.
- **/feedback command** enters teal-accent mode for fire-and-forget session feedback POST.
- **modelId in UserMessageChunk _meta** enables frontend turn-to-model association.
- **Git stash-all** via x.ai/git/stash ACP extension wired to desktop Changes panel.
- **Float timeout_secs** in toolset.bash config.toml enables fractional second timeouts.

## Bug Fixes

- **Silent refresh prevents re-auth** on near-expiry tokens, fixes auth_type propagation and telemetry staleness.


# 0.1.160-alpha.7

## Features

- **Mid-session hook reloading** on trust/untrust/add/remove without restart.
- **Per-turn Stop hooks** plus **UserPromptSubmit** event fire before processing with scrollback annotations.
- **OAuth login option** via --oauth/--legacy flags, GROK_OAUTH_ENABLED env, feature flag (legacy default).

## Bug Fixes

- **Cross-CWD session resume** finds local sessions from worktrees via --resume/--load.

## Performance

- **Sequential N.jpg filenames** replace UUIDs in image_gen for massive token savings.


# 0.1.160-alpha.6

## Features

- **Live auto-refresh** of open hooks/plugins modal on registry changes like install + reload.
- **Adds live PR status display** in desktop changes panel via new ACP x.ai/pr/status extension.
- **Adds domain allowlist and typed WebFetchOutput enum** with DomainNotAllowed and CrossHostRedirect variants.

## Bug Fixes

- **Prevents false-positive background '&' detection** inside heredoc bodies like `cat << EOF... &http.Request... EOF`.
- **Restores web search functionality** by reinjecting required proxy headers like x-grok-client-version.
- **Ensures config.toml base_url fully overrides** default model api_base_url across all credential paths.


# 0.1.160-alpha.5

## Features

- **GrokNight default theme** with **runtime color quantization** ensures correct rendering across truecolor, 256-color, and 16-color terminals.
- **ACP endpoints for hooks/plugins listing** enable pager modals to display loaded hooks and discovered plugins.
- **ACP action endpoints** for hooks/plugins management support trust, install, reload, and update operations via pager.
- **Legacy relay auth flow** via `grok login --legacy` restores pre-OIDC accounts.x.ai token exchange.

## Bug Fixes

- **Terminal-width-constrained tables** wrap cell text proportionally without overflow or column misalignment.
- **Auth fallback to legacy scope** reads old accounts.x.ai tokens on devboxes provisioned via x setup.


# 0.1.160-alpha.4

## Features

- **WebFetch tool** enables secure URL fetching, HTML-to-markdown conversion, SSRF guards, and in-memory caching.


# 0.1.160-alpha.3

## Breaking Changes

- **OIDC OAuth replaces relay auth**; removed `--auth-signin-url`/`exchange-code-url`/`redirect-target` flags, run `grok login` to migrate.

## Features

- **LSP integration** injects diagnostics as reminders and exposes opt-in code intelligence tools via `.grok/lsp.json`.
- **Glob-aware permission rules** support prefix/suffix wildcards, tool=* and refined bash/edit matching.
- **Claude settings.json compatibility** loads legacy rules as fallback when native TOML absent.
- **Path-specific Read permissions** match against file paths from read_file/list_dir tools.
- **Web search model override** via CLI, config.toml or env defaults to grok-4-1-fast-reasoning.
- **Grep-specific permissions** enable path-only matching for Claude-compatible rules.
- **Remote bash timeout toggle** via remote settings auto-backgrounds foreground commands with local override.
- **Image generation tool** creates Imagine API images saved to session/images/ folder.
- **Configurable web search disable** from config.toml via `disable_web_search = true`, OR-ed with CLI flag.
- **Image generation tool** gated behind a server-side flag or `GROK_IMAGE_GEN=1` env var with session auth.
- **Dynamic nested skill discovery** in `.grok/skills/` subdirs with runtime system reminder announcements.

## Bug Fixes

- **Quit confirmation** responds immediately on second ^D without infinite delay.
- **Terminal restore** disables raw mode before CSI sequences to prevent garbage output.
- **Session summary** falls back to prompt prefix on tool parse failure avoiding crash.
- **Prevents YOLO auto-approval** of plan reviews by switching ExitPlanMode to independent ext_method channel.


# 0.1.160-alpha.2

## Features

- **Configurable permission policies** enable automatic allow/deny rules via ~/.grok/config.toml before existing checks.
- **Prevents timeout doom loops** by auto-backgrounding foreground commands exceeding default 120s without explicit timeout.
- **ACP git extensions** enable desktop git info/branches/checkout without local shelling, supporting cloud workspaces.
- **Richer hook annotations** add HTTP URL/status/response previews to scrollback for pre-tool-use summaries.

## Bug Fixes

- **Robust session cancellation** shares logic with 5s safety-net timer to prevent stuck working states.
- **Suppresses config warning** for [desktop] section owned by grok-desktop using opaque serde sink field.
- **Prevents hook reload panics** by wrapping registry in Arc to avoid RefCell borrows across awaits.

## Performance

- **6x faster worktree creation** skips redundant LFS smudge filters on BTRFS snapshots using GIT_LFS_SKIP_SMUDGE=1.


# 0.1.160-alpha.1

## Features

- **New TUI slash commands** /clear alias, /context usage breakdown, /version, and /login auth status.
- **Skills discovery expands** to.agents/skills directories alongside.grok and.claude.

## Performance

- **Fast btrfs worktree creation** on rootless devboxes via gRPC delegation to explorer agent.


# 0.1.160

## Breaking Changes

- **OIDC OAuth2 replaces legacy relay login** for `grok login`; add `--legacy` flag for old flow and remove `--auth-signin-url` etc. CLI flags (migrate by deleting auth.json).
- **Adds `grok login --legacy`** fallback; removes `--auth-signin-url` etc flags (use env/config defaults, no migration needed for most users).

## Features

- **ACP git extensions** (`x.ai/git/info`, `/branches`, `/checkout`) enable cloud-local git ops without subprocesses.
- **Detailed hook scrollback** shows HTTP POST URLs, status codes, and response previews per executed hook.
- **Remote bash timeout config** via remote settings `auto_background_on_timeout` overrides local config.toml.
- **ACP hooks/plugins listing** (`x.ai/hooks/list`, `/plugins/list`) exposes loaded configs for pager modals.
- **Image generation tool** via xAI Imagine API saves to session/images/ folder.
- **Management actions for hooks/plugins** via x.ai/{hooks,plugins}/action with live modal refresh.
- **disable_web_search now honors config.toml** value ORed with CLI flag.
- **WebFetch tool scaffold** securely fetches URLs to markdown with SSRF protection.
- **Dynamic discovery of nested skills** from.grok/skills/ subdirs at runtime via reminders.
- **PR status extension (x.ai/pr/status)** fetches branch PR state, title, number, and URL via gh CLI.
- **Web fetch domain allowlist support** with enum outputs for blocked domains and cross-host redirects.

## Bug Fixes

- **Readable markdown tables** in narrow terminals via proportional column shrinking and smart cell text wrapping.
- **Improved permission UX** by reordering 'don't ask again' option first across TUI, pager, and desktop clients.
- **Reliable prompt cancellation** via shared logic and 5s safety-net timer prevents stuck 'working' states.
- **Silent [desktop] config handling** consumes grok-desktop section without spurious 'unrecognized key' warnings.
- **Panic-free hook reloads** via Arc-wrapped registry prevents BorrowMutError during concurrent plugin updates.
- **Clean terminal restore** disables raw mode before CSI sequences to avoid garbage on Ctrl+D exit.
- **Robust session summaries** handle chat completion errors gracefully without panics.
- **Eliminates compiler unused import warnings** by scoping test imports to cfg-gated function.
- **Prevents YOLO auto-approval of exit plans** by switching to independent ext_method.
- **Fixes web search 426 errors** from missing client version header in requests to the API proxy.
- **Config.toml base_url fully overrides** default models' api_base_url for all credential sources.

## Performance

- **6x faster worktree creation** by skipping redundant LFS smudge filters on snapshot checkouts and resets.


# 0.1.159-alpha.11

## Breaking Changes

- **Default model updated** enables seamless checkpoint swaps; migrate by renaming `[internal_models]` to `[models]` in config.toml.

## Features

- **New hooks-remove and hooks-untrust commands** with shared helpers across shell and TUI.
- **Bash mode (`! cmd`)** enables direct shell execution from pager, bypassing agent loop.
- **Subagents default_model config** forces all subagents to one model, overriding others.

## Performance

- **Faster exit-plan-mode** by sending plan content inline, eliminating client readFile round-trip.
- **Longer inference idle timeout** default raised to 10min for complex responses.


# 0.1.159-alpha.10

## Breaking Changes

- **Default model updated** enables seamless checkpoint swaps; migrate by renaming `[internal_models]` to `[models]` in config.toml.


# 0.1.159-alpha.9

## Breaking Changes

- **Config validation warns** on unknown keys with typed sections; migrate `auto_update` etc. to `[cli]`, `[models]`.

## Features

- **Resolved model ID** now shown in `/session-info` output and feedback submissions.
- **Model sees task completions** as `<system-reminder>` tags in tool results, eliminating polling loops.
- **Ctrl+R fuzzy history search** in pager prompt supports Up/Down navigation and mouse.

## Bug Fixes

- **Consistent tool/parameter names** in model-facing text and errors via TemplateRenderer resolution.
- **Single progress lines** during remote session restore, eliminating duplicates.
- **Plan mode UI updates** correctly on agent entry and across session switches.
- **Compaction succeeds** for Responses API models by preserving encrypted reasoning.


# 0.1.159-alpha.8

## Features

- **Question-answer panel** in pager with **GrokBuildPlanNoSubagents** mode excluding subagent tools.
- **New hook events** Stop, Notification, UserPromptSubmit, SubagentStart/Stop plus **Claude-compatible** toolUseId schema.
- **Resume completed subagents** via task `resume_from` inheriting raw transcript, tool state, model; schema/docs/provenance/observability.
- **`/clear` slash command** aliases `/new` to start a fresh session.


# 0.1.159-alpha.7

## Features

- **Install and uninstall plugins** via `/plugins install <url/path>` and `/plugins uninstall <name>` supporting git refs, subdirs, local symlinks, and multi-plugin repos.

## Bug Fixes

- **Prevents delimiter deletion** in hashline_edit replace by clarifying inclusive anchor/end_anchor range in tool docs.
- **Prevents 413 errors** by compressing large `read_file` images to max 1024px JPEG under 768KB with progressive quality reduction.
- **Eliminates TUI model jump** by patching leader initialize response to use client's `default_model` instead of agent's.


# 0.1.159-alpha.6

## Features

- **Execute inline hooks and MCP servers** from plugin manifests with shell `/plugins add/remove` and HTTP handler support.
- **Redesigned OAuth consent page** uses icon cards; shell callback pages now styled with success/error feedback.
- **MCP catalogs expose scope_name** for human-readable labels alongside scope/scope_id.
- **Discover skills from**.claude/skills directories alongside.grok/skills.
- **TUI hook messages render** inline as scrollback annotations with hashline aliases.

## Bug Fixes

- **use_tool now normalizes** double-encoded JSON strings in tool_input to objects for reliable MCP dispatch.
- **Hashline anchor docs/errors** now specify LINE:HASH1:HASH2 format accurately.
- **Codex grep_files uses** shared rg_path() matching grok_build pattern.
- **Safe truncation prevents** mid-character cuts in subagent/sampling previews.
- **insert_after empty content** now inserts blank line matching docs.
- **Config watcher deduplicates** events per debounce batch for stability.


# 0.1.159-alpha.5

## Features

- **Dev tool usage stats pane** shows real-time activity breakdown, timeline, and inter-token latency.
- **Claude frontmatter parity** parses allowed-tools lists/strings, model, and effort in skill frontmatter.
- **Auto-injected managed MCPs** from grok.com for WebLogin, deduped with config.toml opt-out.
- **ACP worktree management** adds list/show/gc/db methods with filters and dry-run.
- **ACP x.ai/auth/logout** removes scopes from ~/.grok/auth.json.
- **Custom npm registry** via config.toml or GROK_NPM_REGISTRY respects enterprise.npmrc.
- **CLI worktree commands use ACP** with repo-wide session resolution for -w -r.
- **Repo-wide worktree session resume** resolves locally across same-repo directories via ACP before remote fallback.
- **TUI worktree mode** uses repo-wide ACP resolution for interactive and headless resume.

## Bug Fixes

- **Legacy search_replace** skips nearest-match hint computation matching confusable_hint gating.
- **Shorter run_terminal_cmd error messages** revert verbose background & operator wording.
- **fsnotify forwards.git events** to watch_git clients including HEAD/index changes.


# 0.1.159-alpha.4

## Features

- **ACP worktree management** adds list/show/gc/db methods with filters and dry-run support.

## Bug Fixes

- **Legacy search_replace** skips nearest-match hint for 0.4.10 clients matching confusable_hint pattern.


# 0.1.159-alpha.3

## Features

- **Subagent worktrees preserved** after completion with path in output; role-level fork and isolation defaults added.
- **Always-approve mode** renames yolo flag/slash command with backward-compatible aliases preserved.
- **Help skill reads ~/.grok/config.toml** to answer MCP server and model configuration queries.
- **Deployment keys supported** on all API proxy endpoints including storage and sessions.
- **Public install-grok.sh script** supports deployment keys and channels without VPN.
- **Subagent prompts** use dedicated compact template with system role/persona.
- **SubagentSessionMetadata v1** enables GCS persistence with full provenance and registry extensions.

## Bug Fixes

- **Subagents inherit parent file toolset** resolving hashline vs standard configuration correctly.
- **Subagent forking fixed** with system prompt injection, conversation loading, and provenance tracking.
- **Subagent personas override model/reasoning** with full precedence chain and PromptMode::Extend for built-ins.
- **TUI no longer stuck after auto-compact** by preserving original turn ID.
- **Blocks nested subagent spawning** by limiting depth to 1.
- **Subagent permission prompts show tool details**; hashline_edit now requires approval.
- **Forked subagent context normalized** to System + BackgroundContext + Task for recency.
- **Subagents inherit parent yolo mode and OTEL trace**; depth limited to prevent recursion.
- **Unified skill resolution** rejects ambiguities across shell, TUI, backends with qualified names and alternatives.
- **Prevents TUI session errors** on leader connect by skipping eager new session creation.
- **Multi-edit diff previews** for hashline_edit tool parse details array in TUI.
- **Fixes subagent terminal stalls** using spawn_local on LocalSet single-threaded runtimes.
- **Prunes orphaned kill_task/get_task_output** after capability filtering removes providers.

## Performance

- **Eliminates hyper DispatchGone errors** with 2 idle connections and 90s pool timeout.


# 0.1.159-alpha.2

## Features

- **Namespaced plugin skills and agents** enable qualified resolution with deduping and plugin provenance.
- **/plugins slash commands** add list, reload, trust with plugin config in config.toml.
- **Remote default_model** resolution falls back silently if unavailable in user list.
- **Subagent permission attribution** enriches TUI dialogs and events with child provenance.
- **Subagent lineage tree structures** enable recursive depth-sorted display in TUI tasks panel.
- **Structured subagent details and lineage** render diagnostics below selected TUI tasks rows.
- **TUI /plugins commands** support live reload, qualified autocomplete, and plugin source display.
- **Subagent safety guards** fallback to parent model on unknown config or fork context overflow.
- **Bundled /help skill** extracts README.md to ~/.grok/ for slash command and model self-help.
- **Identifies worktree sessions** in client sidebars via session_kind and source_workspace_dir metadata in summary.json.

## Bug Fixes

- **Per-edit diff metadata** prevents cascade from line shifts in hashline multi-edits.
- **Prevents SQLITE_BUSY errors** during concurrent SQLite WAL setup by setting busy_timeout before journal_mode.


# 0.1.159-alpha.1

## Features

- **Subagent persona support** enables layered instructions from config.toml and.grok/personas/*.toml with fail-closed resolution.
- **Compaction-safe fork inheritance** preserves parent prefix while summarizing only child-owned suffix.
- **Enhanced task tool guidance** coaches model on fork_context, capability_mode, overrides, and personas.
- **External auth providers** support custom login binaries with TUI/headless flows.
- **Configurable feedback/trace endpoints** route enterprise telemetry independently.
- **Subagent capability enforcement** filters disallowed built-in tools at spawn time.
- **--no-memory flag** disables cross-session persistence overriding all other configs.
- **Subagent diagnostics** in TUI tasks panel display fork source, capability mode, and persona.
- **Worktree isolation** enables subagents to edit without affecting parent workspace.
- **A/B retry** discards failed forks and replays prompt via new ACP extension method.
- **Synchronous user questions** via client RequestPermission block agent until answered.
- **User approval** for exit_plan_mode via RequestPermission blocks until build or revise.
- **Reasoning deltas** streamed to thinking panel for real-time visibility.

## Bug Fixes

- **Fork-safety filtering** removes synthetic messages and truncates incomplete turns from subagent inherited context.
- **Compact ACP diff metadata** returns only changed lines for scattered edits spanning over 80 lines.
- **Per-client terminal/FS routing** prevents misrouting in leader mode across mixed clients.
- **BYOK model api_keys** preserved during proactive auth refresh by skipping session token overwrite.
- **Unified turn_number** across remote traces and backend storage enables reliable data joins post-rewind.
- **Clean error messages** for session restore failures in non-git directories without backtraces.
- **Stable slash autocomplete** prevents tick refreshes overwriting command suggestions.
- **Explicit AuthType** gates refresh to prevent overwriting user-provided api_keys.


# 0.1.159

## Breaking Changes

- **Default model updated**; rename `[internal_models]` to `[models]` in config.toml.

## Features

- **Server-controlled default model** via remote settings with CLI/config fallback.
- **Subagent capability modes** enforce toolset filtering at spawn time.
- **--no-memory flag** disables cross-session persistence overriding all configs.
- **Subagent metadata** shows fork source, capability, and persona in TUI.
- **Worktree isolation** runs subagents in private git worktrees.
- **A/B retry handler** replays failed dual forks with same prompt.
- **Permission dialogs** attribute requests to child subagents.
- **Lineage tree builder** groups subagents by parent for diagnostics.
- **Subagent detail pane** shows diagnostics and child lineage tree.
- **Synchronous user questions** intercept ask_user_question tool to block agent on client answers via RequestPermission.
- **TUI plugin management** adds /plugins list/reload/trust with live hook reload and CLI --plugin-dir.
- **Interactive plan approval** intercepts exit_plan_mode to block until client Build/Revise.
- **Question-answer panel** adds pager UI for interactive permissions and subagent-free GrokBuildPlan agent.
- **Live reasoning deltas** stream raw text chunks to thinking panel.
- **Dev tool stats pane** docks below scrollback with timeline, ITL p50/p99, and category breakdowns.
- **Skills reject ambiguous short names** with qualified alternatives listed.
- **Subagents use dedicated prompt template** excluding persona catalog.
- **Subagent role/persona now in system prompt** for durable behavioral identity.
- **Skills parse Claude frontmatter** for allowed-tools, model, effort overrides.
- **Plugin install/uninstall commands** support git repos, tags, subdirs, local paths.
- **Auto-injects managed MCPs** from grok.com into CLI/TUI for WebLogin users.
- **ACP worktree management** adds list/show/gc/db methods with filters and dry-run.
- **ACP logout method** removes scopes from ~/.grok/auth.json.
- **Discovers skills from.claude/skills** alongside.grok/skills across local, repo, and user directories.
- **Inline TUI scrollback annotations** for hooks with ✓ success indicator and hashline_* Claude aliases.
- **New hook events** Stop/Notification/UserPromptSubmit/SubagentStart/Stop with Claude-compatible PreToolUse schema.
- **Exposes resolvedModelId** in feedback submissions and /session-info slash command.
- **Warns on unrecognized config.toml keys** at startup using serde_ignored and typed sections.
- **/clear slash command alias** starts new session like /new.
- **Full resume command** shown on TUI exit including cd and --resume with session ID.
- **Git staging awareness** in hunk-tracker with batch get-all-file-contents endpoint.
- **Direct bash mode** (`! cmd`) in pager bypassing agent for shell execution.
- **Global subagent default_model** config overrides all other model sources.

## Bug Fixes

- **Preserves BYOK api_keys** during proactive session token refresh.
- **Unified turn_number** across traces and DB enables reliable remote data joins.
- **Clean errors** on session restore without backtraces or panics.
- **Slash autocomplete** no longer clobbered by tick-driven arg refreshes.
- **Prevents token refresh overwriting** user-provided API keys via explicit AuthType tracking.
- **A/B forks strip reasoning** from assistant messages to prevent CoT leakage.
- **Per-edit diff metadata** captures regions directly avoiding cascade in multi-edits.
- **TUI no longer stuck streaming** after auto-compact turn_id desync.
- **Prevents recursive subagent spawning** by limiting depth to 1.
- **Subagent permission prompts** now display full tool details and paths.
- **Forked subagents get normalized prompts** as [System, BackgroundContext, Task].
- **Leader TUI avoids session conflicts** by skipping eager new session.
- **TUI renders multi-edit diffs** correctly for hashline_edit tools.
- **Subagent tool stalls fixed** via spawn_local for terminal actor.
- **Prevents subagent toolset failures** by pruning orphaned kill_task/get_task_output after capability filtering.
- **Skips nearest-match hint computation** for legacy 0.4.10 search_replace clients.
- **Shortens run_terminal_cmd & error messages** by removing redundant background clause.
- **Fixes deployment key auth** in feedback/sampling clients and external endpoint headers.
- **Normalizes double-encoded tool_input** strings to objects in use_tool dispatch.
- **Clarified hashline anchor formats** to LINE:HASH1:HASH2 in tool descriptions, docs, and errors.
- **Codex grep_files uses shared rg_path** matching grok_build and opencode patterns.
- **Char-boundary-safe truncation** prevents invalid UTF-8 in subagent, sampling, and prompt previews.
- **insert_after empty content inserts blank line** matching tool docs and replace behavior.
- **Deduplicated config watcher events** within debounce batches prevents flaky rapid-write tests.
- **hashline_edit docs specify inclusive replace range** preserving endpoint delimiters like }.
- **Compresses read_file images to JPEG max 1024px/768KB** preventing oversized payload 413 errors.
- **Frictionless desktop auto-auth** by refreshing auth_manager from disk in initialize().
- **Eliminates duplicate summary** in BackgroundTaskStarted tool output.
- **Suppresses duplicate reminders** after kill_task or completed get_task_output.
- **Prevents false non-git warnings** from unexpected libgit2 errors.
- **Blocks unsafe pipelines** by tree-sitter parsing all command segments.

## Performance

- **Fewer `DispatchGone` errors** via larger HTTP pool (2 idle connections, 90s timeout).
- **Inline plan content** in exit_plan_mode avoids client readFile round-trip.


# 0.1.158-alpha.14

## Features

- **Full-text search across past sessions** via x.ai/session/search ACP method with FTS5 indexing and workspace filtering.
- **ACP resume session in worktree** via x.ai/git/worktree/resume_session for programmatic clients.

## Bug Fixes

- **Scoped subagent cancellation** targets only current turn with TUI confirmation modal.
- **Prevents corrupted post-session diffs** by awaiting hooks before root repo replication.
- **Fast AB cancellation** with async hook awaits and 30s timeouts to avoid hangs.
- **Compact snippets for scattered edits** using per-region views with gap markers over 80 lines.
- **Large session sharing** via signed GCS URLs bypassing 413 payload limits.

## Performance

- **Efficient subagent progress updates** with coalescing, adaptive 1-5s polling, stale indicators, and elapsed time display.


# 0.1.158-alpha.12

## Features

- **Live subagent progress tracking** in TUI tasks panel via ACP `list_running`/`get` polling and push notifications.
- **Configurable bash tool params** like timeouts injected from config.toml `[toolset.bash]` into GrokBuild actors.

## Bug Fixes

- **Precise io::ErrorKind mapping** from ACP file errors enables consistent NotFound and PermissionDenied handling.
- **Silent grok-pager installation** hides internal details from install and auto-update user output.
- **Improved batch edit errors** explicitly state atomic all-or-nothing semantics and retry guidance.


# 0.1.158-alpha.11

## Features

- **Configurable hashline toolset** with mutual exclusion validation, scheme parameters, and dynamic descriptions.
- **Multi-source Claude-compatible hooks** from settings files and directories with project trust gating and unified /hooks commands.
- **Live subagent progress** shows turns, tool calls, token usage, tools used, and errors while running.

## Bug Fixes

- **SSE stream parsing** handles flat error format from Grok proxy without deserialization failure.
- **Grep and list_dir outputs** use display paths to prevent leaking internal worktree paths to model.


# 0.1.158-alpha.10

## Features

- **Plan mode state machine** integrates into session lifecycle with persistence and tool support.
- **Post-A/B session hooks** via GROK_AB_POST_SESSION_HOOK capture worktree changes.

## Bug Fixes

- **Prevents Linux Docker overlayfs hangs** in A/B sessions using git-based worktree replication.
- **Eliminates false tool timeouts** for long-running builds after fixing overlayfs hangs.
- **Fixes grep large-output gRPC errors** by raising tools-server limit to 128 MiB.


# 0.1.158-alpha.8

## Features

- **Enhanced announcement UX** adds paging, prev/next commands, and `GROK_DEV_ANNOUNCEMENTS` override.
- **`--disable-web-search` flag** omits web search tool from agent for benchmark isolation.
- **`hashline_read` tool** outputs files with line anchors using chunk h=3 c=8 scheme.
- **Optional auth for feedback** enables unauthenticated submissions with `GROK_USER_METADATA`.
- **Plan mode tools** add `enter_plan_mode`, `exit_plan_mode`, and `ask_user_question` with notifications.
- **Auto-installs grok-pager** alongside grok during internal and GitHub release updates.
- **Mid-session token refresh** re-runs external auth or uses OIDC refresh_token on expiry.
- **`hashline_edit` tool** supports replace/insert_after/write with anchor validation and overlap checks.
- **Tiered range warnings** for hashline edits caution on medium/large multi-line rewrites.
- **Shifted-anchor recovery** in hashline edits suggests retry anchors, reports ambiguities, with wider context.
- **Anchor-annotated grep** injects stable anchors into ripgrep output for seamless edit workflows.
- **Hooks v0 system** runs project scripts on pre/post-tool-use and session lifecycle events.
- **Independent feedback gating** via GROK_FEEDBACK_ENABLED separates it from telemetry controls.
- **Leader CLI tooling** supports discovery, targeting, profiling commands, dev spawn.

## Bug Fixes

- **Bash `run_terminal_cmd` respects `enabled_background=false`** by hiding `is_background` in schema and rejecting at runtime.
- **Structured CLI errors** in tool-server with code, message, and retryable fields.
- **Async metadata** in list_dir prevents blocking the executor on overlayfs-backed paths.
- **Overlay path rewriting** in AcpSessionFs guards AB isolation against display path leaks.
- **ESC cancel recovery** clears cancelling state on prompt-complete notifications.

## Performance

- **Runtime CPU profiling foundation** enables leader process profiling via control protocol and pprof.


# 0.1.158-alpha.7

## Features

- **Managed MCPs flag** resolves env>config>remote settings>default, disabled in headless mode.
- **Grok 4.20 default model** with CLI/env/config/remote-settings overrides for catalog models.

## Bug Fixes

- **Prevents queued prompts flushing together** in TUI via turn IDs ignoring stale responses.
- **Fixed viewport height** enables full prompt dropdown expansion in one frame.
- **Respects explicit yoloMode=false** per-session overriding client defaults.
- **Aborts path walks on first timeout** preventing 25-minute hangs on slow filesystems.
- **5-minute tool timeouts** prevent registry lock hangs with execution tracing spans.

## Performance

- **Tuned HTTP client for IC** adds keepalives, disables Nagle, sets fast connect timeout.
- **Streams request bodies directly** to IC without buffering on common non-rewrite path.


# 0.1.158-alpha.6


# 0.1.158-alpha.5

## Breaking Changes

- **Context window requirement** forces BYOK users to explicitly set it in config.toml, with migration via Serde error messages.

## Features

- **Dynamic model context updates** ensure the chat shell uses the latest limits from the backend for accurate operations.
- **Custom models endpoint** lets users configure their own OpenAI-compatible proxy for enterprise needs.
- **Configurable AB turn timeout** enables dynamic limits via remote settings without redeploying.
- **Automatic file re-reading after compaction** injects recent file contents for immediate model context.
- **Persisted memory reminders** maintain context in chat history for improved session continuity.
- **Timeout for AB comparisons** cancels stalled sessions after a set time, improving reliability.
- **Configurable memory injection** enables controlling first-turn searches for better session starts.

## Bug Fixes

- **Alpha channel updates** now correctly select the latest version, fixing issues with stale releases.
- **Non-blocking memory flushes** ensure sessions progress without delays during idle operations.
- **Accurate prompt completion handling** prevents errors from being misreported as cancellations.
- **Stable application startup** fixes crashes from nested runtimes during initialization.
- **Correct A/B session context windows** ensure auto-compaction uses the right thresholds.
- **Reliable TUI startup** eliminates panics from nested runtimes.
- **Async file handling** prevents session hangs on slow filesystems in AgentsMdTracker.
- **TUI layout fixes** keep prompt anchored and status visible for smoother user interaction.

## Performance

- **Faster SSE error detection** reduces CPU overhead by avoiding full JSON parses on normal chunks.
- **Reused HTTP connections** speed up turns by eliminating TLS handshakes between requests.
- **Single-pass request serialization** cuts overhead for large conversations in streaming paths.


# 0.1.158-alpha.4

## Features

- **Remote announcements** surfaced from remote settings with tolerant parsing, periodic refresh, expiry filtering, and persistent hide/show state.
- **`grok completions` subcommand** generates bash/zsh completion scripts, with fast-path exit before any network or auth warmup.
- **Running subagents preserved after compaction** with IDs, types, and descriptions injected into the post-compaction system reminder.
- **External auth provider** delegates login to a user-supplied binary for sandboxed VMs, CI, and air-gapped environments with automatic mid-session token refresh.
- **Unicode confusable resilience** across search_replace, read_file, and doom-loop paths — smart quotes and em-dashes no longer cause silent edit failures.

## Bug Fixes

- **use_tool works in all server contexts** by dispatching through ToolCallContext instead of session-actor interception, fixing silent failures in grok-tools-server.
- **MCP tool calls use fresh auth** by looking up the current client at call time, fixing silent 401s after OAuth token refresh.
- **Remote settings forwarded through proxy** — telemetry, trace upload, tool search, and writeback flags were silently dropped and never reached the client.
- **Leader startup fetches remote settings** so telemetry, doom loop, and other remote-settings-gated features work in leader mode.
- **AuthManager uses configured proxy URL** instead of a hardcoded default proxy host for user-info fetches.
- **Stale todo spinners cleared at turn end** via transient Plan notification that marks lingering in_progress items as completed for display.
- **Config.toml model overrides applied correctly** — custom api_key, env_key, and base_url on built-in model keys were silently dropped by the enum's per-variant field layout.
- **AB comparison cancel cleanup** now uses canonical teardown path, fixing overlay mount and FS isolation registration leaks.
- **Connect timeout for sampling clients** (10s default, configurable) prevents frozen CLI on unreachable inference servers; TLS warmup now targets the sampling path.
- **Leader protocol backwards compatibility** by defaulting the `ready` field when connecting to older leader binaries.
- **Custom skill path discovery** now works end-to-end — paths added via `x.ai/skills/add` or config.toml are included in agent skill lookup.


# 0.1.158-alpha.2

## Features

- **Configurable telemetry destinations** via `[telemetry]` config section and env overrides, with a trace-upload kill switch for noisy GCS warnings.
- **Pull-on-miss session restore** fetches remote sessions when not found locally, with remote-settings-gated writeback for TUI sessions.
- **Session restore with dedup archives** materializes GCS-referenced patches, blobs, and untracked files before applying.
- **Binary file attachments** persisted to session storage with content-hash dedup and surfaced as path hints for model context.
- **Subagent support** gated behind `--subagents` flag, env var, config, or remote settings — disabled by default.
- **Non-git-repo startup warning** with blocking quit/continue prompt, gated behind a server-side feature flag.
- **Line-numbered memory_get output** matching read_file format, with config-backed search defaults from `[memory.search]`.
- **Richer session auto-save** captures tool-usage breakdown and file paths touched; shell commands excluded to avoid persisting secrets.
- **`grok memory reindex` and `doctor` CLI commands** for index maintenance, plus access-frequency boost in hybrid search.
- **Cursor-based session reconnect** skips already-seen replay events, forwarding only post-cursor updates as live.
- **Subagent support in web UI** with clickable session cards, persistent spawn/finish notifications, and structured completion output.
- **Session restore progress tracking** with phase-level events, elapsed timers, and explicit incomplete-vs-complete outcome differentiation.
- **Per-subagent model routing** via `[subagents.models]` config and agent definition `model` field, with conditional override for heavy parent models.
- **Transparent stdio bridge reconnect** replays cached `initialize` and `session/load` after leader restart so external clients resume immediately.
- **Leader/client version mismatch notification** surfaces a TUI banner and headless log warning when client and leader binaries diverge.
- **OIDC manual paste fallback** races stdin against the loopback server so remote VM users can paste auth codes directly, with `[auth]` config alias.
- **Session ID clipboard copy** on /session-info, with a transient status banner for success and error feedback.
- **Telemetry defaults to off** with centralized env > config > remote settings > default precedence for telemetry and trace uploads.
- **User-defined subagents** via .grok/agents/*.md files with per-subagent config toggles and dynamic Task tool descriptions.

## Bug Fixes

- **Doom-loop detection hardened** with whitespace-normalized fingerprints, per-file failure tracking, nearest-match hints, and outcome-aware error streaks.
- **Restore no longer overwrites git identity** — synthetic commits use scoped env vars instead of writing to repo-local git config.
- **Session token routing fixed** for default models — `grok login` users no longer get 400 errors on proxy-routed models.
- **Richer API error diagnostics** with redacted headers, request body previews, and response metadata in failure messages.
- **Worktree list preserves full IDs** by computing dynamic column width instead of truncating to 16 characters.
- **Restored remote sessions create distinct local children** with parent tracking, preventing identity reuse and duplicate restores on repeated `grok -r`.
- **Auto A/B testing works on non-macOS** by treating absent worktree pool as passthrough instead of gate.
- **Worktree ID collision eliminated** by switching from time-based UUID v7 prefix to random UUID v4.
- **Cancelled and restored sessions no longer hang** by sending explicit shutdown commands before dropping session handles.
- **Memory search config and watcher correctly applied** — `[memory.search]` was silently ignored and first-use sessions missed watcher startup.
- **Auto-continue prompts excluded from real user query counts** in compaction, memory hooks, and session-end telemetry.
- **Deleted memory files removed from search results** and appended content indexed immediately without watcher restart.
- **Doom-loop detection overhaul** with polling-aware stagnation tracking, turn-scoped resets, targeted per-context warnings, and richer error classification.
- **A/B overlay path sanitization** across system prompt, tool results, URL-encoded paths, error messages, and `get_task_output` command display.
- **MCP server retry on init failure** by restoring HTTP config after handshake errors instead of permanently dying.
- **Sessions no longer hang on stalled inference** — content-aware idle timeout distinguishes keepalive SSE events from real completion tokens.
- **Turn completion signaled after empty-response retry** by emitting a fallback `AgentMessageChunk` when streaming events were lost.
- **Up-arrow no-op on empty history** prevents entering history search mode when no previous prompts exist.
- **Safer conversation compaction** by stripping orphaned tool results and falling back when validation fails.
- **Reliable prompt persistence** by gating persist_ack on chat history acceptance and flush barrier completion.
- **Leader startup deadlock resolved** by releasing the file lock before connecting so the leader can reach readiness.

## Performance

- **Faster session restore** by downloading codebase, memory, and state archives concurrently instead of sequentially.
- **Bounded hunk tracker memory** — binary and >1MB files tracked without retaining content, fixing 1.2GB retention from large files.
- **Non-blocking telemetry uploads** prevent prompt turns from stalling when the GCS proxy is unhealthy.
- **Reduced conversation query overhead** with narrow targeted queries that avoid full O(n) conversation clones.
- **Faster session resume** via selective prompt extraction that skips full deserialization of non-prompt update lines.
- **Bounded long-session memory** via eager in-memory pruning of old tool results after each user turn.


# 0.1.158-alpha.1

## Breaking Changes

- **Repeated failed edits now trigger reread guidance** and search_replace NoMatchesFound responses changed shape to include file_path; update ACP/tool-output parsers.
- **Bounded hunk-tracker memory usage** adds explicit file-content status views to the ACP response; clients should migrate from legacy content fields to `baseline` and `current` metadata.

## Features

- **Configurable telemetry routing** with custom event endpoints, Mixpanel controls, and remote kill switches for analytics and trace uploads.
- **Remote session restore on local miss** by hydrating backend sessions into local storage and gating writeback sync with a server-side flag.
- **More complete remote restores** by materializing deduplicated patches and blobs before replay, with partial-restore warnings for external users.
- **Binary file attachments are now supported** by decoding blob resources to session storage with MIME metadata, size limits, and content-hash deduplication.
- **Subagent task execution** adds child agent sessions with lifecycle tracking, progress rendering, and background-task management across the CLI stack.
- **Opt-in subagent spawning** adds a `--subagents` flag plus env, config, and remote settings controls to gate the task tool.
- **Safer startup outside repositories** with a feature-flagged non-git warning that lets users quit before losing git-backed tracking and rewind features.
- **More useful memory tooling** with line-numbered memory_get, config-backed memory_search defaults, clearer memory docs, and richer auto-saved summaries that omit shell commands.
- **Memory maintenance commands** with reindex/doctor workflows and search ranking that lightly boosts frequently retrieved memories.
- **Versioned tool contracts** with preset-selected legacy behavior for run_terminal_cmd and read_file, plus contract metadata for integrators.
- **Subagent transcripts in web** with persisted status cards, structured completion data, and reload-safe child session replay.
- **Per-subagent model routing** with config and agent-definition overrides, plus correct inheritance from each parent session's live model.
- **Transparent stdio reconnect recovery** by replaying cached `initialize` and session state after leader restarts, then notifying clients.
- **Remote-VM sign-in fallback** by accepting pasted OIDC redirect URLs or tokens when the browser runs on another machine.
- **One-click session ID copying** from session info, with a transient status banner and clipboard error reporting.
- **Telemetry stays off by default**, with env, config, and remote settings precedence for telemetry and trace uploads.

## Bug Fixes

- **Stronger repeated-edit detection** by normalizing whitespace in search_replace fingerprints so indentation-only retries now trigger doom-loop warnings.
- **Faster edit recovery after misses** with nearest-match hints in search_replace errors and tighter guidance to use minimal unique anchors.
- **Session restore no longer overwrites repo git identity** by scoping synthetic commit author details to the restore subprocess only.
- **Error-loop termination is more reliable** by tracking per-file tool failures separately and only resetting warnings after successful writes.
- **Longer repeating tool cycles are detected** by raising doom-loop cycle length coverage and using deterministic fingerprint hashing.
- **Logged-in ACP clients can use default fast models again** by routing session tokens through the chat API proxy instead of api.x.ai.
- **API failures are easier to diagnose** with redacted request and response context in sampling errors and improved error propagation.
- **Full worktree IDs in listings** prevent truncated UUIDs and hashes, with dynamic column sizing in `worktree list` output.
- **Restored sessions keep local lineage** by creating a new local child session and tracking the remote parent for repeatable resumes.
- **Auto A/B works on Linux and other non-macOS systems** by falling back to on-demand worktree creation when no pool exists.
- **Concurrent worktree creation avoids collisions** by switching temporary worktree IDs from time-based UUID prefixes to random UUID v4 values.
- **Cancelled and restored sessions stop getting stuck** by routing prompt-complete notifications correctly and sending explicit shutdowns during teardown.
- **More reliable cross-session memory** by sharing one backend configuration, filtering synthetic prompts, fixing delete and append reindexing, and correcting telemetry counts.
- **Cursor-aware reconnect replay** skips already processed updates and resumes session streams from the last seen event.
- **Fewer false doom-loop stops** with smarter polling detection, per-turn resets, targeted warnings, and synthetic warning tags.
- **A/B comparison path isolation** so prompts, tool results, and task output no longer expose overlay worktrees or wrapper commands.
- **Recoverable MCP startup failures** by restoring HTTP configs for retries and surfacing readable MCP App handshake errors.
- **Clearer restore progress and failure states** with phased events, elapsed times, and explicit incomplete-session-state reporting.
- **Hung streaming requests now fail fast** by timing out keepalive-only SSE streams that stop producing real model content.
- **Leader startup no longer races client connects** by binding the IPC socket early and gating ACP traffic on explicit readiness.
- **More reliable A/B filesystem isolation** with a configurable non-overlay scratch base before falling back to tmpfs mounts.
- **Clients can distinguish auto-update restarts** by propagating explicit leader shutdown reasons through the IPC protocol and reconnect state.
- **Completed turns no longer disappear downstream** by emitting a fallback text chunk when streaming events were lost after retries.
- **Leader/client version mismatches are surfaced immediately** with ACP notifications, TUI banners, and headless warnings after registration.
- **Prompt history search no longer opens empty** by ignoring Up-arrow history mode when both queued prompts and saved history are absent.
- **Legacy list_dir compatibility** for older clients, restoring empty-directory, error, and depth-threshold output parity.
- **Safer conversation compaction** by stripping orphaned tool results and falling back when replayable history validation fails.
- **More reliable prompt persistence** by acknowledging prompts only after chat history accepts them and the flush barrier completes.
- **Legacy task error parity** for older clients, restoring exact not-found wording in get_task_output and kill_task.

## Performance

- **Faster session restore** downloads codebase, memory, and state archives concurrently while surfacing clearer restore strategy and warning summaries.
- **Prompt turns stay responsive** by moving GCS telemetry uploads to fire-and-forget paths when retries or queue fallbacks occur.
- **Lower memory use in long sessions** by replacing full conversation clones, selectively scanning updates, and pruning retained tool results.
- **Faster startup before code navigation** by lazily building indexes only for web clients that advertise x.ai/codeNavigation.


# 0.1.158

## Features

- **Remote announcements** from remote settings with tolerant deserializer.
- **Shell completions** via `grok completions <shell>`, leader CLI, and stdio reconnect replay.
- **User-defined subagents** via.grok/agents/*.md with config toggles.
- **Post-compaction reminder includes running subagents** with IDs, types, and poll/cancel instructions.
- **External auth provider binary** enables login via custom commands in sandboxed/air-gapped environments.
- **Custom models_base_url auto-fetches** OpenAI-compatible model list for enterprise proxies.
- **Guarded normalized fallback matching** enables search_replace on confusable Unicode typography (gated by flag).
- **Full-text search across sessions** via new `x.ai/session/search` ACP extension with FTS5 indexing.
- **Retained file context post-compaction** by re-reading up to 5 recent files into history.
- **Persisted memory reminders** upsert into conversation system prompt without duplicating transient injections.
- **Configurable AB turn timeout** cancels stalled comparisons after wall-clock limit with cleanup and observability.
- **Configurable first-turn memory injection** with dedicated thresholds and remote settings support.
- **Managed MCPs feature flag** skips fetching by default in headless mode.
- **Paged announcement UX** adds prev/next commands with reliable startup visibility.
- **Disables web search tool** via `--disable-web-search` flag or `GROK_DISABLE_WEB_SEARCH=1` for benchmarks.
- **hashline_read tool** annotates file output with anchors like `LINE:LOCAL:CONTEXT→CONTENT`.
- **Grok 4.20 default model** with CLI/env/config/remote-settings overrides for web search and summaries.
- **Unauthenticated feedback submission** via optional tokens and `GROK_USER_METADATA` env.
- **Plan mode tools** EnterPlanMode, ExitPlanMode, AskUserQuestion send structured notifications.
- **Auto-installs grok-pager** alongside grok during internal and GitHub release updates.
- **Mid-session token refresh** via OIDC grants or external auth binaries prevents expiry.
- **Hashline toolset** enables anchor-stable file read/edit/grep with validation, recovery, ranges, and config integration.
- **Hooks system** executes custom scripts for pre/post-tool and session events from ~/.grok/hooks/ with deny-wins trust controls.
- **Independent feedback flag** gates /feedback and popups separately from telemetry via GROK_FEEDBACK_ENABLED.
- **Leader CLI** adds `grok leader list/info/profile/kill/dev` for discovery and CPU profiling.
- **Plan mode state machine** enables agent planning phase with enter/exit tools and session persistence.
- **Hooks from Claude settings** loads from `~/.claude/settings.json` and project `.claude/settings.json`.
- **Live subagent progress** shows turns, tools, tokens, errors in `get_task_output` for running tasks.
- **Unified `/hooks` command** supports `list`, `trust`, `add <path>` with fuzzy autocomplete.
- **ACP `x.ai/subagent/list_running`** queries live progress for all running subagents of parent session.
- **Bash tool params from config.toml** override schema defaults like `timeout_secs` for GrokBuild.
- **Live subagent progress** in TUI tasks panel via ACP polling and push notifications.
- **ACP extension resumes sessions in worktrees** via single call matching `grok -w -r` CLI flow.
- **Signed GCS uploads for shares** bypass proxy limits to prevent 413 errors on large sessions.

## Bug Fixes

- **Prompt persistence ack** gated on chat history flush barrier completion.
- **Leader spawn deadlock** resolved by releasing lock before connect.
- **Headless clipboard failures** handled silently in session info.
- **Server-side flags forwarded** via proxy for telemetry and other features.
- **Leader fetches settings** enabling remote-settings-gated runtime features.
- **AuthManager respects configured proxy URL** instead of hardcoding the default proxy host.
- **Clears stale todo spinners** at turn end with transient Plan notification without mutating underlying state.
- **Config.toml model overrides now apply** custom api_key/base_url to built-in models.
- **Prevents queued prompts flushing together** via per-turn ID guarding race between notification and response.
- **Alpha updates pick max(stable, alpha)** and harden channel admissibility across all installers.
- **Unicode confusables now detected and normalized** in search_replace/read_file with diagnostics, reminders, and guarded fallback matching.
- **Fixes overlay mount and FS isolation leaks** on AB comparison cancel by using canonical cleanup.
- **Adds connect timeout to sampling clients** and warms TLS roots for first-chat cold-start latency.
- **Leader protocol compatibility** by defaulting `ServerMessage::Registered.ready` to true.
- **Custom skills now discoverable** via config.toml paths and `x.ai/skills/add` in system prompts.
- **Non-blocking memory flushes** by spawning idle-timer tasks asynchronously.
- **Accurate turn end display** by not defaulting missing prompt-complete stop reasons to cancelled.
- **Correct error display** for retries by checking explicit `Cancelled` stop reason only.
- **Startup panic eliminated** by replacing nested tokio runtime with direct await.
- **Correct auto-compact in AB forks** by overriding context_window from target model registry.
- **Async FS operations** in AgentsMdTracker prevent hangs on overlayfs-backed filesystems.
- **Fixed TUI viewport layout** anchors prompt at bottom with reliable status bar.
- **run_terminal_cmd exported schema** hides and rejects `is_background` when disabled via params.
- **Fixed viewport height** enables single-frame dropdown resize without animation.
- **Respects explicit per-session yoloMode=false** overriding client defaults in leader and TUI.
- **Aborts path walks on first filesystem timeout** preventing 25-minute hangs on slow mounts.
- **5-minute tool execution timeout** prevents registry hangs with diagnostic tracing spans.
- **Non-blocking list_dir** uses tokio::fs::metadata to avoid executor hangs on overlayfs-backed paths.
- **A/B fork writes** are guarded by display-to-overlay path rewriting in AcpSessionFs.
- **ESC cancel** resets [cancelling] state via relaxed prompt-complete guard.
- **Linux A/B forks** skip overlayfs and replicate via git diff to prevent container syscall hangs.
- **No tool timeouts** allows long-running builds without false positives.
- **Matches tools server gRPC limits** to 128 MiB preventing `OutOfRange` on large grep outputs.
- **SSE parsing handles flat errors** from Grok proxy alongside OpenAI-standard nested format.
- **ACP file errors map to `io::ErrorKind`** ensuring consistent `NotFound`/permission dispatch.
- **Display paths in grep/list_dir** prevent internal worktree leaks to model in A/B forks.
- **Silent grok-pager installation** hides implementation details from install/update output.
- **Improved hashline_edit batch errors** state atomicity and retry-all guidance.
- **Scoped subagent cancellation** targets only current-turn subagents.
- **Post-session hooks await before** replication avoids root-repo diff corruption.
- **AB cancellation unblocked** via async hooks and cleanup timeouts.
- **Per-region snippets for scattered edits** cap output at ~40 lines instead of 10K+ for distant changes.

## Performance

- **SSE error detection skips JSON parse** on normal chunks via fast contains("\"error\"") guard.
- **Faster LLM turns** by persisting HTTP/2 client with connection pooling across conversation turns.
- **Faster streaming completions** by single-pass serialization replacing serde Value mutation.
- **Tuned reqwest client** enables keepalives, nodelay, and timeouts for faster IC backend requests.
- **Streamed request bodies** skip buffering and parsing for large non-rewritten payloads.


# 0.1.157-alpha.1

## Features

- **Subagent support** gated behind `--subagents` flag, `GROK_SUBAGENTS` env var, config.toml, or remote settings — disabled by default.

## Bug Fixes

- **Cancelled and restored sessions no longer hang** by sending explicit shutdown commands before dropping session handles.

## Performance

- **Faster session restore** by downloading codebase, memory, and state archives concurrently instead of sequentially.


# 0.1.157

## Features

- **Remote session restore on miss** fetches session data from backend when not found locally, with remote-settings-gated writeback.
- **Dedup-referenced codebase content materialised during restore** by downloading GCS-backed patches and blobs before replay.
- **Binary file attachments** decoded from base64, written to session storage with content-hash dedup, and surfaced as path hints to the model.
- **Subagent spawning and lifecycle management** with coordinator tracking, TUI progress rendering, and task tool integration.
- **Non-git directory warning** with blocking confirmation modal at startup, gated behind a server-side feature flag.
- **Line-numbered `memory_get` output** matching `read_file` format, with `memory_search` defaults now respecting `[memory.search]` config.
- **Richer session-end auto-saves** now include tool-usage breakdown and file paths touched; shell commands excluded to prevent credential leakage.
- **`grok memory reindex` and `doctor` CLI commands** for index maintenance, plus access-frequency boosting for retrieved memory chunks.

## Bug Fixes

- **Doom-loop detection hardened** with whitespace-normalized fingerprints, per-file failure tracking, nearest-match hints, and error-streak termination.
- **Session restore no longer overwrites git identity** — synthetic commits use subprocess env vars instead of repo-local config.
- **Session-authenticated users no longer get 400 errors** on default models — credential routing checks actual token source, not advertisement method.
- **API error messages include structured context** — request URL, relevant headers, and body preview surfaced on auth, payload, and server failures.
- **Worktree list displays full IDs** with dynamic column width instead of truncating to 16 characters.
- **Memory search config honored** — `[memory.search]` settings were silently ignored across all three backend construction paths.
- **Correct memory injection after compaction** — first-turn context now uses the real user query, not the auto-continue prompt.
- **Deleted memory files no longer searchable** — watcher sync removes stale chunks, and `/memory append` content is indexed immediately.

## Performance

- **Reduced session memory pressure** by replacing 8 full-conversation clone sites with narrow single-field actor queries.
- **Faster session resume** via selective `updates.jsonl` scanning that skips full deserialization of irrelevant update types.
- **Bounded long-session memory** by eagerly pruning old tool results from the retained conversation after each user turn.


# 0.1.156

## Features

- **Per-session tip rotation** via a persistent cursor so each launch shows the next tip in sequence instead of the same UTC-day tip.
- **Cross-devbox session restore** via `grok sessions list/search` CLI subcommands and hardened `grok -r` with upload-ordering safety, cwd-scoped lookups, and staged-vs-unstaged correctness.

## Bug Fixes

- **Scoped MCP connector selection** so multiple connectors with the same URL but different auth scopes (Personal/Team/Org) route to the correct token.
- **GCS upload reliability** with panic-catching upload tasks, full error-chain logging, and aggressive HTTP connection pool eviction to prevent stale-connection retries.
- **Doom loop detection for repetitive edits** that were previously invisible because interleaved bash commands reset the detector.
- **Binary blob size cap** enforced during archive building, preventing multi-GB binaries from attempting uploads that would timeout.
- **Graceful upload drain on shutdown** so pending GCS uploads complete before exit instead of being silently abandoned.
- **Malformed tool-call JSON sanitization** replaces invalid arguments with `{}` before sending to providers, preventing permanent 400 loops from broken model output.
- **MCP tool name validation** skips tools with invalid characters (e.g. spaces) that cause Anthropic/OpenRouter 400 errors, instead of crashing the session.
- **Patch restore compatibility with older git** by removing `--allow-empty` flag and pre-checking for diff content before calling `git apply`.

## Performance

- **Streaming multipart upload for large files** (>50 MB) bypasses proxy body limits and avoids loading entire files into memory.


# 0.1.155-alpha.6

## Features

- **Tip of the day** shown at startup, served dynamically from remote settings with local opt-out via `[cli] show_tips = false`.
- **OAuth2 Authorization Code + PKCE** login flow replacing legacy relay auth, with server-controlled rollout and permanent legacy fallback.

## Bug Fixes

- **Gitignore enforcement** on `read_file` and `search_replace` to reduce accidental secret exposure — aligns with `list_dir` and `grep` behavior.
- **Session resume race fix** — prevents `--resume` from creating a shadow session when `LoadSession` is in-flight via leader IPC.
- **Atomic summary writes** via temp-file + rename — fixes EOF parse errors when A/B comparison startup races with summary updates.
- **Memory tool labels** — `memory_search` and `memory_get` now render as "Memory Search" and "Memory Read" instead of generic labels.
- **HTTP/2 connection poisoning fix** — rebuilds the sampling client on final retry to recover from stale connections after server GOAWAY/RST_STREAM.


# 0.1.155-alpha.5

## Features

- **Tip of the day** shown at startup from remote settings, rotated daily; opt out with `[cli] show_tips = false`.
- **Gitignore-aware file tools** — read_file and search_replace now refuse access to ignored paths, matching grep and list_dir.

## Bug Fixes

- **Session resume no longer races or silently creates a new session** when leader IPC or replay fails.
- **Atomic summary.json writes** prevent EOF parse errors during concurrent A/B comparison session loads.
- **Memory Search and Memory Read labels** in TUI instead of generic Search/Read for memory tool calls.
- **HTTP/2 connection poisoning recovery** via idle timeout eviction and fresh client rebuild on final retry attempt.
- **Bash command output appears immediately** by flushing the replay buffer before turn end in `!` prefix mode.


# 0.1.155-alpha.4

## Features

- **End-to-end distributed tracing** across clients and the API proxy via W3C `_meta.traceparent` propagation into a single distributed trace.

## Bug Fixes

- **Structured error variants** for read_file, list_dir, search_replace, and todo_write so callers can distinguish file-not-found, permission-denied, and duplicate-id failures.
- **Replay notifications now guaranteed before session/load response** via `forward_with_completion()` drain; removes public `acp_send_fire_and_forget` from xai-acp-lib.
- **Writeback sync now includes cwd and title** in backend metadata on every flush and rename, fixing null values in remote session listings.
- **Typed TaskNotFound errors** for kill_task and get_task_output with known-task-ID hints, enabling Python-side structured error classification.
- **Terminal commands detached from controlling TTY** via `setsid()` so subprocesses like ssh cannot steal input from the TUI.


# 0.1.155-alpha.3

## Features

- **MCP Apps support** for rendering interactive UI tools served via `ui://` resources from MCP servers.
- **OpenID Connect login** with PKCE, local callback server, and configuration via config.toml or environment variables.
- **On-demand MCP tool discovery** via BM25 search and `use_tool` meta-dispatch for KV cache-stable definitions.
- **Kill background tasks by task_id** via new `x.ai/task/kill` ACP extension method.
- **Integration tool names visible in TUI** instead of generic search_tool/use_tool plumbing labels.

## Bug Fixes

- **Rewind correctness overhaul** — mutation-free preview, ghost message filtering, compaction-aware replay for all targets.
- **Memory flush and compaction no longer fail** on orphaned tool_result messages by stripping tool blocks before summarization.
- **Slash command autocomplete enters argument phase** instead of executing immediately, and option-key word traversal corrected.
- **Cancel cleanup awaits child process exit** to reclaim memory before returning, preventing cascading OOM kills.

## Performance

- **Parallel tool execution** via shared resources and explicit ToolCallId, removing the sequential single-tool bottleneck.


# 0.1.155-alpha.2

## Features

- **Config hot-reload** watches auth, MCP servers, memory, skills, UI, and models for live changes without restart.
- **`/memory` slash command** appends notes to workspace or global MEMORY.md with smart Markdown heading normalization.
- **cgroup v2 memory limits for spawned commands** gracefully OOM-kill only the offending process, keeping the session alive.
- **Debugging technique retention** across sessions by capturing API endpoints, CLI commands, and investigation workflows in memory flush.
- **File overwrite guard** on search_replace prevents empty old_string from silently replacing existing file contents.
- **Full command preview in scrollback** for long or multi-line bash commands that exceed the status bar.

## Bug Fixes

- **Duplicate tool name validation** prevents unreachable tools when two share the same client-facing name.
- **Large pasted text written to disk** so the model's read_file fallback finds the content instead of file-not-found.
- **TUI reconnect no longer hangs permanently** after leader restart — 30s timeout and guaranteed completion signal.
- **Duplicate leader exits cleanly** with 30s lock timeout and early bail-out when an existing socket is detected.
- **A/B fork session traces** now include repo_root and remote_url via shared git2 discovery logic.
- **Distinct fs tool error messages** differentiate 'does not exist' from 'is a file, not a directory' to prevent agent loops.
- **Drain timeout for inherited pipes** prevents `cmd &` without stdout redirect from blocking the actor loop indefinitely.
- **Leader log level defaults to info** instead of debug, cutting log volume ~10x over long-running sessions.
- **Memory flush sanitization** strips tool_result blocks before windowing to prevent orphaned tool_use_id 400 errors.
- **A/B comparison disk leak fix** — correct overlay unmount ordering prevents 4.9 GB leak, and primaryModelId now reflects the actual variant model.
- **Revert memory flush sanitization** that stripped tool messages and broke flush request formatting.

## Performance

- **Faster first prompt on large repos** by capping startup git_status at 2s instead of blocking 10-20s on index refresh.
- **Dedup blob existence pre-check** skips redundant uploads when another agent has already written the same SHA256 content.
- **Session eviction on client disconnect** drops SessionHandles to reclaim ~100-500MB per session when IPC clients leave.
- **mimalloc allocator on macOS Apple Silicon** for alpha builds, fixing 15GB+ RSS from system allocator's unreturned pages.


# 0.1.155

## Bug Fixes

- **HTTP/1.1 fallback for sampling requests** when HTTP/2 connections fail with transport errors from GOAWAY or RST_STREAM frames.
- **Handle response.failed and error streaming events** from the Responses API instead of silently dropping them.
- **Prevent file descriptor exhaustion on macOS** by capping parallel walker threads and raising the soft FD limit at startup.


# 0.1.154-alpha.6

## Features

- **Overlay-based A/B replication** replaces fsnotify+hunk-tracker with overlayfs to capture all terminal-created files during A/B sessions, fixing lost-file bugs on Linux.
- **/flush slash command** in the TUI triggers an on-demand memory flush to disk via the existing backend extension method.


# 0.1.154-alpha.5

## Features

- **Tool execution tracing** via call_id passthrough, follow-up messages, and structured error detail fields in CLI output.
- **Mandatory AGENTS.md discovery** — runtime-discovered project instruction files must now be read before proceeding.
- **PTY notification routing** to originating client via targetClientId, with cwd fallback and terminal UI polish.
- **`--no-auto-update` flag** for leader mode to prevent update-triggered shutdowns in long-running sessions.
- **On-demand memory flush** via `/flush` command and `x.ai/memory/flush` extension with concurrent-flush prevention.
- **Tool state export/import** via GetToolState RPC and initial_tool_state_json, enabling session warm-start and cloning.
- **`GROK_MEMORY=0` force-disables memory** regardless of config.toml or remote settings; CLI flag still overrides.
- **Per-tool MCP timeout overrides** via `tool_timeouts` in config.toml and `toolTimeoutsMs` in ACP session meta.

## Bug Fixes

- **Slash command completion** now respects optional arguments, fixing `/load`, `/model`, `/compact`, and `/theme` execution.
- **Per-session model, sampling, and YOLO tracking** in leader mode, preventing cross-client state contamination.
- **Complete session replay on reconnect** by switching to unbounded client channels, preventing silently dropped notifications.
- **Zombie process prevention** on leader respawn with child reaper, config.toml auto_update respect, and stale-binary resolution.
- **OOM prevention in codebase indexer** via 5MB file-size cap, binary detection, and hidden-directory filtering.
- **Per-client YOLO mode isolation** in leader mode — toggling no longer contaminates other clients' sessions.
- **A/B merge-back recovers files in new directories** created via terminal, fixing the inotify recursive-watch race.
- **Worktree cwd preserves subdirectory offset** instead of resetting to the repo root.
- **8 MiB stack for session threads** prevents stack overflow on macOS where the default is 512 KB.
- **Grep tool description clarifies raw regex syntax** to prevent the model from quoting patterns.
- **Tool kind params merge across all tools** of the same ToolKind, fixing template render failures for multi-tool configs.
- **User query placed before attached file contents** in the prompt for better model attention.
- **Read file errors now include the file path and underlying cause** across all tool implementations.

## Performance

- **Lightweight index status queries** via GetFileCount/GetStats, eliminating a full ScopeGraphIndex clone on every status check.
- **Hunk tracker skips unnecessary file reads** in AgentOnly mode and eliminates a redundant content clone.


# 0.1.154-alpha.4

## Features

- **Background command lifecycle** reworked with graceful SIGTERM→SIGKILL escalation, non-blocking I/O, and proper agent tracking for user-backgrounded commands.

## Bug Fixes

- **Auto A/B comparisons deferred until worktree pool is ready**, preventing 30-60s stall on large repos.
- **Headless mode (`grok -p`) exit panic** fixed by flushing telemetry before runtime teardown.


# 0.1.154-alpha.3

## Features

- **MCP server management** via mcp/list, mcp/call, and mcp/servers_updated extension methods with multi-scope connector resolution.

## Bug Fixes

- **PTY notifications reach the correct client** via _meta routing metadata, with shared helpers and session-aware cwd fallback.
- **Concurrent tool calls no longer crash** the tools server; semaphore serializes access to the thread-local toolset.
- **Cross-session notification leaks eliminated** in leader mode for relay, dead-client, and ext/notification routing paths.
- **Leader auto-update pre-downloads the binary** before shutdown and resolves the ~/.grok/bin symlink at spawn time.
- **First-compaction memory flush fires correctly** by pre-incrementing the compaction counter before the flush guard.
- **Interval memory flush resumes after compaction** by resetting the conversation length counter when history is compacted.
- **Session content restored on TUI reconnect** by clearing stale scrollback, resetting turn state, and gating live updates during replay.
- **Memory flush entries accumulate across cycles** by appending to daily log files instead of overwriting each flush.
- **A/B experiment tool output shows original project paths** instead of internal worktree directories in streaming updates.
- **File write truncation eliminated** by switching to tokio::fs and disabling ACP filesystem routing from the TUI.
- **Token refresh works for JWTs with aud claims** by disabling audience validation during expiration parsing.
- **Session resume and fork no longer crash** with "Is a directory" error caused by a regression in the Tool trait migration.


# 0.1.154-alpha.2

## Features

- **Structured headless prompts** via --prompt-json and --prompt-file flags, with --verbatim to skip query wrapping.
- **Bring-your-own-key for A/B comparisons** by reading [ab] openrouter_api_key from config.toml.


# 0.1.154-alpha.1

## Features

- **Mid-session MCP server toggling** via x.ai/session/update_mcp_servers extension method with optimistic rollback.
- **Sandbox profile configurable via GROK_SANDBOX env variable**, defaulting to workspace profile on devboxes.
- **Agent version exposed in InitializeResponse** metadata for relay and client version discovery.
- **`grok worktree` subcommand** for listing, inspecting, removing, and garbage-collecting session worktrees.
- **Character-budget BFS directory listing** replaces depth-based summarization, expanding small deep directories when budget allows.

## Bug Fixes

- **Tool-server finalize subcommand aligned** with the Python client via named --tools/--truncation args and --json output.
- **Completion requirement tracking restored** in recovery loop, fixing duplicate tool calls introduced by the DynTool migration.
- **Foreground command cancellation no longer deadlocks** — terminal backend access moved outside the registry mutex.
- **A/B session filesystem isolation fixed** by replacing overlayfs with bind-mounts, eliminating split brain between bash and file tools.
- **ACP file operations** were silently bypassing the client filesystem backend, breaking ask-mode write rejection in GrokCode.
- **Zero-argument MCP tool calls** no longer fail with JSON parse error when the model emits an empty arguments string.
- **Model selection in leader mode** now persists across sessions — changing models mid-session no longer reverts on /clear.
- **Git index.lock contention on devboxes** resolved by adding --no-optional-locks to all background git operations.
- **System reminders** now fully respect the disabled flag, and template parameter defaults auto-populate from input schemas.
- **Memory tools now render in TUI and web UIs** with search/read cards, query text, and file path display.

## Performance

- **Session thread isolation** via per-session tokio runtimes, preventing tool execution in one session from blocking another.


# 0.1.154

## Features

- **`/flush` slash command** in the TUI for on-demand memory flush to disk.

## Bug Fixes

- **A/B session file preservation** via host-side overlayfs, fixing terminal-created files silently lost during merge-back.
- **Memory flush no longer infers preferences** from unchallenged assistant actions, requiring explicit user statements.


# 0.1.152-alpha.1

## Features

- **Slash command autocomplete** with ACP-advertised built-in and skill commands, keyboard navigation, and fuzzy filtering.
- **Compact and rewind controls** exposed to the web frontend via typed SessionClient methods with backwards-compatible camelCase aliases.
- **OpenCode agent mode** with 8 dedicated tools (bash, read, edit, write, grep, glob, todowrite, skill) using opencode conventions.

## Bug Fixes

- **Codex agent tool accuracy** restored by switching to dedicated codex-specific read, list-dir, and grep implementations.
- **File editing on macOS** fixed by canonicalizing search_replace paths to match read_file's case-normalized tracker keys.
- **Workspace disambiguation in /load** prevents session list overwrites when multiple workspaces share the same directory basename.
- **ACP agent profile support** restored for JSON object payloads, fixing permission mode and prompt loss for web and remote clients.


# 0.1.151

## Features

- **Interactive web terminals** let clients run full PTY shells over ACP WebSocket with resize, reconnect replay, and unified terminal lifecycle APIs.
- **Configurable Codex system prompts** let builds toggle custom base templates per agent definition while preserving correct agent-type resolution from the active default model.

## Bug Fixes

- **Graceful tool cancellation handling** prevents agent-loop aborts when gRPC calls are cancelled, with structured CANCELLED errors and end-to-end cancellation coverage.
- **Accurate edit preview line numbers** now appear before approval by sending diff metadata through standard tool_call_update events instead of permission payload plumbing.


# 0.1.149

## Features

- **Offline relay session sync** keeps shared sessions resilient with reconnect and persisted cursors, adding connection-state notifications for reliable TUI status updates.
- **Foreground tool progress visibility** shows active tool execution in the TUI status bar, generalizing streaming state beyond bash commands.
- **Config-driven skill loading** lets users add and ignore custom skill paths from config.toml, extending discovery beyond default local and user directories.
- **Persistent leader availability after disconnects** by spawning leader with --no-exit-on-disconnect, allowing new clients to reconnect without restarting the subprocess.

## Bug Fixes

- **Reliable CLI updates** now refresh grok-latest during installs, ensuring symlink-based setups launch the newly installed binary in fresh shells.
- **Parallel tool-call stability** prevents Anthropic ordering errors by deferring follow-up user messages until all tool_result blocks in a batch are emitted.
- **Channel-aware update checks** correctly install alpha builds during channel switches by combining semver comparison with explicit stable-versus-alpha mismatch logic.
- **More reliable tool execution in forks** validates concatenated JSON against the named tool schema and preserves FileReadTracker state by copying tool_state.json.
- **Client disconnect cancellation** now stops running tools and triggers cleanup for foreground commands in unary and streaming execution paths.
- **Stable A/B fork path behavior** keeps model-visible project paths consistent while resolving stale absolute paths safely to worktrees and preserving edit-read tracking state.
- **Reliable session resume across providers** by repairing dangling tool calls throughout conversation history, preventing persistent 400 errors on interrupted legacy sessions.
- **Accurate replayed terminal state** by skipping streaming bash update handling during replay, preventing ghost in-progress timers from out-of-order persisted chunks.
- **Cleaner TUI tool-call logs** by sending no placeholder raw_input until tagged ToolInput arrives, eliminating repeated 'missing field variant' parse errors.


# 0.1.146

## Features

- **A/B filesystem isolation** prevents cross-session contamination by running forked sessions in overlayfs and syncing winner side-effects safely.
- **A/B safety guardrails** now block unsafe side-effecting commands during comparisons and abort contaminated experiments before execution.

## Bug Fixes

- **Responsive A/B cancellation** now interrupts worktree preparation and mid-copy operations, avoiding stuck comparisons and stale vote blocking.
- **Background task visibility** now shows immediate execution feedback and caps blocking waits, preventing apparent freezes during long-running commands.


# 0.1.145

## Bug Fixes

- **Reliable hunk acceptance and replication** by refreshing baselines on git state changes and preserving tracked paths after accept/reject workflows.
- **Cleaner client tool event stream** by suppressing updates for skipped tool calls while still persisting cancellation context in conversation history.


# 0.1.144

## Features

- **Offline-safe feedback retention** writes feedback and A/B votes to local session JSONL before network submission, including previously dropped vote rationale fields.
- **Agent environment metadata becomes queryable** by merging GROK_AGENT_METADATA into initialize responses and persisting the full blob in backend agent records.
- **Dynamic leader-mode control** resolves from CLI, local config, then remote settings, and startup also adopts remote upload limits.

## Bug Fixes

- **Complete prompt trace capture** preserves full pre-truncation text in GCS and records truncation metadata for reliable debugging of long prompts.
- **Backward-compatible metadata semantics** distinguish absent legacy prompt flags from explicit false values by serializing truncation/image indicators as optional booleans.
- **Rewind preview no longer crashes** on multibyte text by truncating at UTF-8 character boundaries instead of raw byte offsets.
- **Large file attachments are safely bounded** by emitting metadata stubs above a token threshold, preventing context blowups while preserving file discoverability.
- **Non-interactive edit workflows** no longer fail read-before-edit checks because tools server defaults `skip_read_before_edit` to true at startup.
- **A/B forks now keep subdirectory context** so comparison sessions run in the same repo subpath and replicate changes correctly.
- **Server-side A/B gating is now enforced** by honoring classification results before forking, reducing unnecessary comparisons for low-value prompts.
- **Forked A/B sessions now inherit current YOLO mode** so tool permissions stay consistent with user state and comparisons avoid approval stalls.

## Performance

- **Faster A/B fork creation** removes backend registration latency from fork setup by spawning session sync as background telemetry work.
- **Lower A/B fork overhead** skips low-value telemetry calls for ephemeral comparison forks, avoiding extra session-load work during experiments.
- **Quicker dual-worktree setup** reuses one precomputed dirty-state scan across both A/B syncs, eliminating redundant git status calls on large repos.
- **Shorter A/B comparison startup** overlaps both fork-and-spawn flows with concurrent joins, reducing sequential network wait in dual-fork initialization.
- **Pool restart latency drops** by adopting valid orphan worktrees instead of recreating them, with tested cleanup and adoption flow hardening.


# 0.1.143

## Features

- **Prompt introspection from the CLI** is available via `grok prompt`, with JSON or section output and persisted session prompt_context snapshots.
- **Prompt image tracing** now captures user-supplied images as decoded per-turn files in GCS, improving multimodal debugging and auditability.
- **A/B comparisons now support ask/plan read-only mode** by forking sessions without git worktrees, enabling comparisons outside repositories with safer non-mutating defaults.
- **Per-server MCP timeouts** can be set via `_meta.mcpConfig`, with relay passthrough and precedence over config defaults during server startup and tool calls.
- **Ask/plan A/B can force worktrees** through `[cli].ab_force_worktrees`, overriding read-only defaults when operators need full filesystem-isolated comparisons.
- **Installation diagnostics** help troubleshoot conflicting grok binaries by listing canonical and invoked paths, versions, update status, and optional JSON output.

## Bug Fixes

- **Reliable todo status updates** now accept merge-only status patches by defaulting missing content to todo IDs instead of rejecting updates.
- **Accurate prompt re-rendering** now respects active tool overrides and disabled tools by centralizing all prompt assembly through a shared PromptContext.
- **A/B cancellation no longer double-finishes comparisons** by suppressing completion notifications and after-uploads when cancel handlers already removed active comparison state.
- **Running outside git repos no longer panics** by skipping gitignore construction without a repo root and guarding absolute-path ignore checks.
- **npm installs now use a canonical Grok binary path** via postinstall copy to `~/.grok/bin/grok`, preventing installer conflicts and version confusion.


# 0.1.142

## Features

- **Hidden directories now appear in file listings** by defaulting fs list requests to include hidden paths and removing unused session fs/git handlers.
- **Faster A/B comparisons in large macOS repos** via reusable prefilled worktree pools, atomic claiming, and preparation signaling.
- **CLI self-update downloads** now use authenticated, time-limited URLs with environment-scoped auth for updates.
- **Prompt context now includes editor focus state** by rendering focused files, open files, and regular resource links in structured system reminders.
- **Richer editor context in prompts** now includes focused and open files as resource links with cursor metadata for better code-grounded responses.
- **More reliable self-updates in restricted environments** by falling back to GitHub Releases via `gh` when npm or internal installers fail.

## Bug Fixes

- **Web search works again** by passing sampling credentials through toolset overrides so the web_search tool is registered correctly.
- **Unique agent identities across containers** by incorporating Linux HOSTNAME into ID derivation and normalizing stored agent IDs to UUIDv5.
- **Embedded file attachments parse more URI formats** by accepting optional file:// prefixes and both #Lstart-end and #Lstart-Lend fragments.
- **Single-@ file references now work in TUI prompts** by broadening reference parsing to support optional @ and L-prefixed line ranges.
- **Compacted conversations preserve query structure** by wrapping summaries in `<user_query>` tags so resumed sessions keep consistent context formatting.
- **Duplicate session messages are prevented** by making saveSessionData append only unseen message tails based on persisted message counts.
- **A/B winner changes now land in your main repo** by replicating worktree edits to the original source directory explicitly.
- **Debug extension methods now accept camelCase params** while keeping snake_case aliases, improving client compatibility without breaking existing callers.
- **Fewer stale HTTP client failures** by disabling idle connection pooling for sampling requests and relying on HTTP/2 keep-alive checks.

## Performance

- **Faster worktree pool operations** reduce acquire and cleanup overhead with skip-clean paths and streamlined removal logic.


# 0.1.141

## Breaking Changes

- **Per-model concise behavior** replaces global [toolset].use_concise; migrate by setting model.<name>.use_concise=true because the old global key is ignored.

## Features

- **Faster completion acceptance** lets you confirm prompt, file, and history suggestions with Right Arrow alongside Enter and Tab.
- **Inline code review comments** can now be created and deleted via new extension methods, with append-only GCS event records for pipelines.
- **Machine-readable update checks** add --check and --json support to update and version commands for automation-friendly version status reporting.
- **Automatic project-instruction discovery** surfaces newly encountered AGENTS.md and Claude.md paths during tool access, including post-compaction reminders outside the initial directory chain.

## Bug Fixes

- **Cleaner pager command output** relies on shell no-color mode, removing local ANSI stripping and preserving streamed text exactly.
- **Accurate restored session metrics** persist full SessionSignals snapshots and reload them on resume, avoiding turn-count drift after compaction.
- **More reliable long streaming requests** use per-sampling HTTP clients with keep-alive tuning instead of a shared global client.
- **Startup stability for external instruction files** avoids gitignore panics by skipping ignore checks for paths outside the repository root.
- **Tool-call protocol compliance** now emits cancellation and rejection outputs for unexecuted tools, preventing model errors when expected tool results are missing.


