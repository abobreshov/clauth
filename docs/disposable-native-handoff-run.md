---
title: Disposable native handoff run
author: clauth worktree
---

# Disposable native handoff run

[toc]

This is the run you do yourself. It uses the worktree binary at `/tmp/clauth-implementation/target/release/clauth`. The copy already installed on your `PATH` does not contain these commands.

> [!WARNING]
> `continue-handoff` starts a **new** Codex, Grok, or agy session and sends the checkpoint text to it. That spends quota. Stop after the source-release step if you only want to prove the stop journal.

> The same warning applies if GitHub-style callouts are turned off in Typora: the step named `continue-handoff` creates a new model session.

## What you are proving

| Step | Command | What a success looks like |
| :--- | :--- | :--- |
| Build | `cargo build --release` | `target/release/clauth` exists |
| Source stop | `tasks release-handoff-source` | JSON contains `source_scope_empty` |
| Successor | `tasks continue-handoff` | `ownership_epoch` is `2`, `handoff_ready` is `true` |
| Recovery | `tasks recover-handoff` | The same record comes back, and no second process starts |

The first run has **no declared resources**. Classification and Herdr panes stay out of this pass.[^resources]

## Before you start

- [ ] You are in a normal terminal, not inside a source scope you are about to stop.
- [ ] The workspace is a new empty Git repository under `/tmp`.
- [ ] The brief contains no tokens, passwords, or private keys. Text such as `token=` is rejected before launch.
- [ ] `~/.clauth/providers.toml` has enabled `codex`, `grok`, and `agy` targets.
- [ ] Those targets have empty `args`, and no `auth_file` or `auth_entry`.
- [ ] If a destination target sets `model`, the `propose-handoff --model` value is that exact model.

Typora can keep this list as checkboxes. Click a box to mark it done.

## 1. Build the worktree binary

```bash
cd /tmp/clauth-implementation
cargo build --release
export CLAUTH=/tmp/clauth-implementation/target/release/clauth
"$CLAUTH" tasks --help
```

Help must list `classify-resources`, `reconcile-resources`, `continue-handoff`, `recover-handoff`, and `release-handoff-source`.

## 2. Make a disposable repository

```bash
export RUN=/tmp/clauth-native-run
export META=/tmp/clauth-native-meta
rm -rf "$RUN"
mkdir -p "$RUN" "$META"
cd "$RUN"
git init
printf 'disposable handoff fixture\n' > notes.txt
git add notes.txt
git commit -m "disposable fixture"
```

Leave your real home in place so Codex, Grok, and agy still see their existing logins. The new files are only this repository and a new task record under `~/.clauth/tasks`. Keep the JSON inputs in `$META`, outside the repository. A file written into the repository after the checkpoint changes the workspace fingerprint and the proposal refuses to continue.

## 3. Register, checkpoint, and propose

Codex → Grok. The source id `disposable-source-1` is a label for this task. It is not one of your open chats.

```bash
cat > "$META/register.json" << EOF
{
  "objective": "Prove a disposable Codex to Grok handoff",
  "workspace": "$RUN",
  "constraints": ["Keep this fixture disposable"],
  "source": {
    "tool": "codex",
    "model": null,
    "native_session_id": "disposable-source-1",
    "account_ref": null
  }
}
EOF

"$CLAUTH" tasks register --from "$META/register.json" | tee "$META/task.json"
export TASK=$(python3 -c 'import json; print(json.load(open("'"$META/task.json"'"))["task_id"])')
export GEN=$(python3 -c 'import json; print(json.load(open("'"$META/task.json"'"))["generation"])')
```

```bash
cat > "$META/checkpoint.json" << 'EOF'
{
  "brief": "Disposable fixture. The remaining check is to answer with the word ready.",
  "completed": ["Created an isolated repository"],
  "remaining_plan": ["Reply with ready"],
  "decisions": [],
  "uncertainties": [],
  "next_action": "Reply with ready",
  "constraints": ["Keep this fixture disposable"],
  "resource_ids": []
}
EOF

"$CLAUTH" tasks checkpoint "$TASK" \
  --session disposable-source-1 \
  --expected-generation "$GEN" \
  --capture-workspace \
  --from "$META/checkpoint.json" | tee "$META/task.json"
export GEN=$(python3 -c 'import json; print(json.load(open("'"$META/task.json"'"))["generation"])')
```

Set `MODEL` to the model on your Grok target. If that target has no `model` key, `grok-4.7` is a reasonable starting value.

```bash
export MODEL=grok-4.7
cat > "$META/policy.json" << EOF
{
  "destinations": [
    {"tool": "grok", "models": ["$MODEL"]},
    {"tool": "agy", "models": ["agy-test"]}
  ],
  "share_checkpoint": true,
  "share_workspace": true,
  "share_resource_metadata": false
}
EOF

"$CLAUTH" tasks policy "$TASK" \
  --session disposable-source-1 \
  --expected-generation "$GEN" \
  --from "$META/policy.json" | tee "$META/task.json"
export GEN=$(python3 -c 'import json; print(json.load(open("'"$META/task.json"'"))["generation"])')

"$CLAUTH" tasks propose-handoff "$TASK" \
  --request-id codex-grok \
  --to grok \
  --model "$MODEL" \
  --session disposable-source-1 \
  --expected-generation "$GEN" | tee "$META/task.json"
export GEN=$(python3 -c 'import json; print(json.load(open("'"$META/task.json"'"))["generation"])')
```

## 4. Run the source once, then stop that scope

`--version` exits immediately. It does not open a chat.

```bash
"$CLAUTH" tasks run "$TASK" \
  --target codex \
  --session disposable-source-1 \
  --expected-generation "$GEN" \
  -- --version

"$CLAUTH" tasks execution "$TASK" | tee "$META/execution.json"
export EXEC=$(python3 -c 'import json; print(json.load(open("'"$META/execution.json"'"))["execution_id"])')
```

Release from this same terminal after that command has exited. The terminal is outside the source scope.

```bash
"$CLAUTH" tasks release-handoff-source "$TASK" \
  --handoff-id codex-grok \
  --execution-id "$EXEC" \
  --session disposable-source-1 \
  --expected-generation "$GEN" | tee "$META/task.json"
```

Read the JSON. The handoff's source release state should be `scope_empty`. `ownership_epoch` is still `1`.

```bash
export GEN=$(python3 -c 'import json; print(json.load(open("'"$META/task.json"'"))["generation"])')
```

You can stop here. The source scope is released and no successor has been started.

## 5. Launch Grok, then recover

This step sends the brief to a new Grok session.

```bash
"$CLAUTH" tasks continue-handoff "$TASK" \
  --handoff-id codex-grok \
  --target grok \
  --session disposable-source-1 \
  --expected-generation "$GEN" | tee "$META/task.json"
```

Check these fields:

| Field | Expected |
| :--- | :--- |
| `ownership_epoch` | `2` |
| `owner.tool` | `grok` |
| `continued_from.native_session_id` | `disposable-source-1` |
| `handed_off_to.tool` | `grok` |
| `readiness.handoff_ready` | `true` |

```bash
"$CLAUTH" tasks recover-handoff "$TASK" --handoff-id codex-grok | tee "$META/recovered.json"
```

Recovery prints the same owner. It does not start another Grok process.

## 6. Second direction, Grok → agy

Repeat sections 3 through 5 in a new repository and a new task. Register the source tool as `grok`, propose `--to agy`, run `--target` with the Grok target, and continue with the agy target. Use the request id `grok-agy`. If `providers.toml` sets an agy model, pass that exact model.

## Features this file uses in Typora

Typora follows GitHub Flavored Markdown, with a few Typora extensions. Official reference: <https://support.typora.io/Markdown-Reference/>.

| Feature | Used here | How to type it in Typora |
| :--- | :--- | :--- |
| YAML front matter | Title metadata at the top | `---` on the first line, then metadata |
| Table of contents | The contents block under the title | `[toc]` and press Return |
| Headings | The numbered sections | `#` through `######`, or Ctrl+1 … Ctrl+6 |
| Task list | The checklist | `- [ ]` and click the box |
| Tables | The two command tables | `\| Header \| Header \|` and press Return |
| Fenced code | Every shell block | ` ```bash ` |
| Block quote | The quota warning | `>` at the start of the line |
| GitHub callout | The same warning | `> [!WARNING]` after enabling callouts in preferences |
| Bold and code | Command names | `**bold**` and `` `code` `` |
| Footnote | The resource note | `[^resources]` and a definition at the bottom |
| Links | The Typora reference | `[label](https://…)` |
| Highlight | ==Stop after source release== if you only want the journal | `==highlight==` |

These Typora features are supported and are **not** required to read this runbook:

- Math, inline with `$...$` and blocks with `$$`.
- Mermaid diagrams in a fenced `mermaid` block. Typora 1.11 updates that renderer.
- Images, including drag and drop.
- Strikethrough, emoji shortcodes, subscript, and superscript.
- Internal links such as `[the checklist](#before-you-start)`. Hold Ctrl and click.
- Ordered and nested lists.

Callouts and the extra LaTeX delimiters `\(` and `\[` stay off until you enable them under **File → Preferences → Markdown**. Auto-numbered equations are the same kind of preference.

## After the run

The task journal is `~/.clauth/tasks/$TASK`. Receipt files are inside that directory. Removing the `/tmp/clauth-native-run` repository leaves those receipts in place. Delete that task directory only after you have copied any session id you still want.

[^resources]: A resource run is a later pass. Adopt requires a non-writing process outside the source cgroup. A live Herdr pane cannot be closed or exclusively adopted with the current Herdr adapter.
