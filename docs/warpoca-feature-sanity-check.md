# WarpOCA Feature Sanity Check

Use this file as the pre-ship checklist for the internal WarpOCA macOS app.
Statuses:

- `Pass`: verified in the current WarpOCA build.
- `Needs test`: expected to work, but needs a manual pass before packaging.
- `Needs work`: known replacement or bug work remains.
- `Local replacement`: Warp cloud behavior is replaced by local/OCA behavior.
- `Intentionally disabled`: cloud/team feature is not part of WarpOCA.

Official Warp references used to build this checklist:

- [Getting started with Warp and Oz](https://docs.warp.dev/)
- [Universal Input](https://docs.warp.dev/terminal)
- [Modern Text Editing](https://docs.warp.dev/terminal/editor)
- [Blocks](https://docs.warp.dev/terminal/blocks)
- [Block Actions](https://docs.warp.dev/terminal/blocks/block-actions)
- [Command Search](https://docs.warp.dev/terminal/entry/command-search)
- [Session Management](https://docs.warp.dev/terminal/sessions)
- [Tabs](https://docs.warp.dev/terminal/windows/tabs)
- [Split Panes](https://docs.warp.dev/terminal/windows/split-panes)
- [Launch Configurations](https://docs.warp.dev/terminal/sessions/launch-configurations)
- [Warpify](https://docs.warp.dev/terminal/warpify)
- [Warp Drive](https://docs.warp.dev/features/warp-drive)
- [Workflows](https://docs.warp.dev/features/warp-drive/workflows)
- [Notebooks](https://docs.warp.dev/knowledge-and-collaboration/warp-drive/notebooks)
- [Prompts](https://docs.warp.dev/knowledge-and-collaboration/warp-drive/prompts)
- [Environment Variables](https://docs.warp.dev/features/warp-drive/environment-variables)
- [Agents Overview](https://docs.warp.dev/agents/overview)
- [Using Agents](https://docs.warp.dev/agents/warp-ai/agent-mode)
- [Code Overview](https://docs.warp.dev/code/code-overview)
- [Code Editor](https://docs.warp.dev/code/code-editor)
- [Code Review](https://docs.warp.dev/code/reviewing-code)
- [Session Sharing](https://docs.warp.dev/features/session-sharing)
- [Settings Sync](https://docs.warp.dev/terminal/more-features/settings-sync)

## Release Gate

Do not package `dist/WarpOCA.app` until every item marked `Ship gate: yes` is either `Pass`, `Local replacement`, or `Intentionally disabled`.

| Area | Feature | WarpOCA status | Ship gate | Manual test |
| --- | --- | --- | --- | --- |
| App startup | Launch without Warp sign-in | Pass | yes | Open `target/release/bundle/osx/WarpOCA.app`; it must not open Warp auth or block terminal use. |
| App startup | Uses Codex CLI auth, no OCA key prompt | Pass | yes | Confirm `~/.codex/auth.json` exists, launch app, run `/agent who are you?`; no API key prompt should appear. |
| Proxy | Bundled local proxy starts on `127.0.0.1:1337` | Pass | yes | `lsof -nP -iTCP:1337 -sTCP:LISTEN` should show `WarpOCA.app/Contents/Helpers/byob_proxy`. |
| Proxy | Health endpoint | Pass | yes | `curl -sS http://127.0.0.1:1337/healthz` should show `auth_configured: true` and `wire_api: responses`. |
| Proxy | OCA/Codex config discovery | Pass | yes | Health endpoint should show OCA base URL, `model_override: gpt-5.5`, `auth_configured: true`, and `wire_api: responses`. |
| Proxy | Regression tests | Pass | yes | `cargo test -p byob_proxy` should pass all proxy translation tests. |
| Terminal | Shell command execution | Needs test | yes | Run `pwd`, `echo hello`, `printf 'a\nb\n'`; output should render normally. |
| Terminal | ANSI rendering / colors / Unicode | Needs test | yes | Run `printf '\033[31mred\033[0m\nunicode-ok\n'`; color and unicode should display correctly. |
| Terminal | Files, links, and scripts | Needs test | no | Run `echo https://oracle.com` and `echo README.md:1`; links should be clickable or copyable. |
| Input | Modern editor editing | Needs test | yes | Type a long command, move cursor with arrows/Option-arrow, paste multiline text, clear with `Ctrl-C`. |
| Input | Classic Input / shell prompt compatibility | Needs test | yes | Toggle input style in Settings and confirm zsh prompt, PS1, and command execution remain usable. |
| Input | Completions | Needs test | yes | Type `cd ` then press completion trigger; directories should appear. Type `git che` and verify suggestions. |
| Input | Command history | Needs test | yes | Run `echo history-test`, then press up/history UI; command should be available. |
| Input | Command Search | Needs test | yes | Press `Ctrl-R`, search `history-test`, press Enter; command should be inserted. |
| Input | Next Command | Pass | yes | Run `brew search nvtop`; ghost suggestion should be `brew install nvtop`, not raw JSON. |
| Input | Command Corrections | Needs test | no | Run a typo such as `gti status`; verify correction UI appears if enabled. |
| Input | Synchronized Inputs | Needs test | no | Split pane, enable synchronize current tab, type `echo sync`; text should mirror to both panes. |
| Blocks | Block creation | Needs test | yes | Run `ls`, `echo hello`, and a failing command like `xyz`; each should become a separate block. |
| Blocks | Block failure styling | Needs test | yes | Run `xyz`; block should show non-zero/error styling. |
| Blocks | Copy command/output/both | Needs test | yes | Right-click a block and copy command/output; paste into editor to confirm content. |
| Blocks | Re-input command | Needs test | yes | Use block action to re-input a prior command; it should appear in the input editor. |
| Blocks | Bookmark / navigate blocks | Needs test | no | Bookmark a block, scroll, then navigate back through block actions or keyboard shortcuts. |
| Blocks | Filter block output | Needs test | no | Run a command with many lines, use block filter, and confirm matching lines are shown. |
| Blocks | Attach block to Agent | Needs test | yes | Attach a block to Agent Mode and ask it to summarize the output; answer should use that context. |
| Blocks | Shared block title generation | Pass | no | `POST /ai/generate_block_title` should return a concise title through the local proxy, not Warp cloud. |
| Windows | Tabs | Needs test | yes | Open/close/reopen tabs, rename a tab, switch with shortcuts. |
| Windows | Split panes | Needs test | yes | Split right/down, move focus, maximize pane, close pane. |
| Windows | Drag/drop panes and tabs | Needs test | no | Drag pane between tabs and confirm session survives. |
| Sessions | Session navigation | Needs test | no | Open several tabs/panes, invoke session navigation, search by current command or prompt. |
| Sessions | Session restoration | Needs test | yes | Run commands, quit WarpOCA, relaunch; windows/tabs/panes and recent blocks should restore. |
| Sessions | Working directory for new sessions | Needs test | no | Configure previous/current/custom working directory and open a new tab. |
| Sessions | Launch Configurations | Needs test | yes | Save a simple two-pane launch config from UI, close/reopen it, confirm CWDs and commands. |
| Workflows | YAML workflows | Needs test | yes | Create a local YAML workflow, find it in Command Search/Palette, insert and run it. |
| Workflows | Warp Drive workflow create/edit/run | Needs work | yes | Create a workflow in Warp Drive, edit it, run it, quit/relaunch, confirm it persists locally. |
| Workflows | Workflow arguments and enum prompts | Needs test | yes | Create a workflow with `{{name}}`; insert it and verify argument selection/cycling. |
| Notebooks | Create/edit local notebook | Needs work | yes | Create notebook, add text and shell command block, quit/relaunch, confirm content persists. |
| Notebooks | Run notebook command blocks | Needs work | yes | Create shell block `echo notebook-ok`; insert/run it into active terminal. |
| Prompts | Saved prompt create/run | Needs work | no | Create a prompt, find it through slash/Command Palette, run it through OCA proxy. |
| Env vars | Static env var object | Needs work | no | Create env var object, load into session, verify `echo $VAR`. |
| Env vars | Dynamic env var command | Needs work | no | Create dynamic variable backed by a command, load into session, verify value resolves locally. |
| Warp Drive | Personal local object store | Needs work | yes | Verify objects created through Warp Drive are durable after restart and do not call Warp cloud. |
| Warp Drive | Team workspace / sharing / cloud sync | Intentionally disabled | no | UI should not promise usable team sharing or Warp cloud sync. |
| Warp Drive | Import/export | Needs test | no | Export workflow/notebook/env; import the file back and confirm object renders. |
| Agent | Agent Mode prompt routing | Pass | yes | `/agent who are you?` should route through local proxy/OCA. |
| Agent | Streaming text | Needs test | yes | Ask a longer question; response should stream without duplicate final text. |
| Agent | RunShellCommand tool | Needs test | yes | Ask Agent to run `pwd`; it should produce a native Warp command tool card/action. |
| Agent | ApplyFileDiffs tool | Needs test | yes | In a temp git repo, ask Agent to edit a tiny file; review/apply diff in Warp UI. |
| Agent | Agent permissions | Needs test | yes | Set shell-command permission to ask; Agent should request confirmation before running commands. |
| Agent | Context blocks | Needs test | yes | Attach a failed command block and ask Agent to diagnose it. |
| Agent | Agent query suggestions | Pass | no | After `brew search nvtop`, AM suggestion must be plain text/structured JSON, not duplicated raw JSON. |
| Agent | Partial Agent query prediction | Pass | no | `/agent` or Agent input prediction should return plain text, not raw JSON. |
| Agent | Passive prompt suggestions | Pass | yes | Passive MAA suggestions can now call Warp's native `SuggestPrompt` tool through the local proxy. |
| Agent | Suggested rules chips | Needs test | yes | Ask Agent to infer a durable preference; if it calls `suggest_rule`, Warp should show the native suggested-rule chip and save to local Markdown. |
| Agent | Model list | Pass | yes | Model selector should show only OCA/Codex Enterprise configured models, not Gemini/A100 placeholders. |
| Agent | Web lookup behavior | Needs work | yes | Ask a current-events or docs question; decide whether OCA should answer, refuse, or use an approved internal retrieval path. |
| Code | Built-in code editor | Needs test | yes | Open a file from terminal output or Command Palette, edit it, save with `Cmd-S`. |
| Code | File tree / Project Explorer | Needs test | no | Open a git repo and browse/create/open files from the file tree. |
| Code | Relevant files endpoint | Pass | no | `POST /ai/relevant_files` should rank matching paths locally without Warp cloud. |
| Code | Code Review panel | Needs test | yes | Make a git diff, open Code Review, inspect/revert/open file. |
| Code | Agent-generated code diffs | Needs test | yes | Ask Agent for a one-line file edit; diff UI should appear and apply cleanly. |
| Code | Codebase Context / indexing | Needs work | yes | Verify whether WarpOCA still tries Warp cloud. If so, replace with local/OCA retrieval or disable UI honestly. |
| Code | Project rules / `AGENTS.md` | Needs test | no | Run `/init` in a temp repo and verify local `AGENTS.md` behavior without Warp cloud. |
| Knowledge | Local global rules | Pass | yes | Rules created in the UI are written under `~/Library/Application Support/WarpOCA/rules/*.md` and included as agent context. |
| Knowledge | Local knowledge files | Needs test | no | Drop Markdown files under `~/Library/Application Support/WarpOCA/knowledge`; verify they are included in agent context. |
| Warpify | Subshells | Needs test | no | Enter `zsh` or `bash`; blocks/input/completions should continue after Warpify. |
| Warpify | SSH Warpify | Needs test | no | SSH to an internal test host, accept/cancel Warpify, verify terminal still works and no Warp cloud call is required. |
| MCP | MCP server config | Needs test | no | Add a local MCP server if UI is present; Agent should call it through OCA-compatible tool path or fail cleanly. |
| Appearance | Themes | Needs test | no | Switch theme and restart; setting should persist locally. |
| Appearance | Keybindings | Needs test | no | Remap a harmless shortcut, restart, confirm it persists locally. |
| Cloud | Session Sharing | Intentionally disabled | no | Share-session commands should be hidden, disabled, or clearly unavailable in WarpOCA. |
| Cloud | Block Sharing permalinks | Intentionally disabled | no | Block share actions should not call Warp cloud. Copy/export local text can remain. |
| Cloud | Settings Sync | Intentionally disabled | no | Settings must stay local; no Warp account requirement. |
| Cloud | Teams / invites / billing / account settings | Intentionally disabled | no | Team/account UI should not block terminal use or call Warp auth/cloud APIs. |
| Packaging | App name and bundle | Pass | yes | App displays as `WarpOCA`; bundle contains `Contents/Helpers/byob_proxy`. |
| Packaging | App crate compile sanity | Pass | yes | `PATH="$PWD/target/byob-metal-xcrun:$PATH" cargo check -p warp --features gui` should pass. |
| Packaging | Fresh-machine install | Needs test | yes | On a clean Mac with Codex CLI login, unzip/copy app, launch, run terminal and Agent smoke tests. |
| Packaging | No stale proxy from `dist` | Pass | yes | Before packaging, kill old helpers and verify live helper path is from the package being tested. |

## Quick Smoke Script

Run these against the currently opened direct app before deeper manual UI checks:

```bash
lsof -nP -iTCP:1337 -sTCP:LISTEN
curl -sS http://127.0.0.1:1337/healthz
curl -sS -X POST http://127.0.0.1:1337/ai/generate_input_suggestions \
  -H 'Content-Type: application/json' \
  --data '{"context_messages":["{\"input\":\"brew search nvtop\",\"output\":\"==> Formulae\\nnvtop\",\"context\":{\"exit_code\":0}}"],"history_context":"","system_context":null,"rejected_suggestions":[],"prefix":null,"block_context":null,"previous_result":null}'
curl -sS -X POST http://127.0.0.1:1337/ai/generate_block_title \
  -H 'Content-Type: application/json' \
  --data '{"command":"brew install nvtop","output":""}'
```

Expected Next Command response:

```json
{"commands":["brew install nvtop"],"ai_queries":[],"most_likely_action":"brew install nvtop"}
```

Expected Block Title response:

```json
{"title":"Install nvtop"}
```

## Manual Smoke Flow

1. Launch direct test app: `open -n target/release/bundle/osx/WarpOCA.app`.
2. Run `echo hello`, `pwd`, `xyz`, and `printf '\033[31mred\033[0m\n'`.
3. Confirm separate blocks, copy output, re-input command, and failing-command styling.
4. Run `brew search nvtop`; confirm Next Command suggests `brew install nvtop`.
5. Use `/agent who are you?`; confirm OCA/Codex model and no Warp sign-in.
6. Ask Agent to run `pwd`; confirm native command tool flow.
7. In a temp git repo, ask Agent to edit a tiny file; confirm diff UI and apply behavior.
8. Create one tab, one split pane, save a Launch Configuration, close, reopen it.
9. Create a local workflow and notebook; restart; confirm persistence.
10. Open Command Search and Command Palette; verify history, workflow, notebook, and agent-history entries behave as expected.
11. Add a rule from Settings > Knowledge > Rules; confirm a `.md` file appears in `~/Library/Application Support/WarpOCA/rules`.

## Known High-Risk Areas

- Warp Drive local persistence is currently only partially replaced. The local object client prevents cloud dependency, but durable create/edit/restart behavior still needs real UI testing.
- Codebase Context may still depend on Warp/Oz infrastructure. We should either replace it with local retrieval through OCA/Codex or make the UI clearly local-fallback only.
- Agent web/current-doc lookup needs a policy decision. The app should not silently depend on Warp cloud, and should not pretend it browsed if OCA cannot.
- Cloud collaboration features are intentionally out of scope: session sharing, block permalinks, team Warp Drive, Settings Sync, account/billing.

## Automated Pass Log

### 2026-05-19

- Verified live app process: `target/release/bundle/osx/WarpOCA.app/Contents/MacOS/warp-oss`.
- Verified live helper process: `target/release/bundle/osx/WarpOCA.app/Contents/Helpers/byob_proxy`.
- Verified `127.0.0.1:1337` listener belongs to the live helper.
- Verified bundle name: `WarpOCA`.
- Verified bundle id: `com.oracle.WarpOCA`.
- Verified `/healthz`: OCA base URL, `gpt-5.5`, Responses API, Codex auth configured.
- Verified `/ai/generate_input_suggestions`: `brew search nvtop` -> `brew install nvtop`.
- Verified `/ai/generate_am_query_suggestions`: structured simple suggestion, no duplicated raw JSON.
- Verified `/ai/predict_am_queries`: plain text suggestion, no duplicated raw JSON.
- Verified `/ai/relevant_files`: local ranking returns the matching auth file first.
- Verified `cargo test -p byob_proxy`: 20 passing tests.
- Verified `cargo check -p warp --features gui`: passed.

### 2026-05-20

- Added proxy support for Warp native `SuggestPrompt` tool calls.
- Added proxy support for suggested-rule `ShowSuggestions` actions via `suggest_rule`.
- Added `/ai/generate_block_title` to the local proxy and routed shared block title generation away from Warp cloud.
- Added local global rule sources: `~/Library/Application Support/WarpOCA/rules/*.md` and `~/Library/Application Support/WarpOCA/knowledge/*.md`.
- Changed Rules UI and suggested-rule saves to write local Markdown rules instead of cloud AI facts.
- Verified patched proxy on `127.0.0.1:1338`: `/healthz`, `/ai/generate_input_suggestions`, and `/ai/generate_block_title`.
- Verified `cargo test -p byob_proxy`: 23 passing tests.
- Verified `PATH="$PWD/target/byob-metal-xcrun:$PATH" cargo test -p ai project_context`: 25 project-context tests passing.
- Verified `PATH="$PWD/target/byob-metal-xcrun:$PATH" cargo check -p warp --features gui`: passed.
