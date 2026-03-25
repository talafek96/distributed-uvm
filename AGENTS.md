

# Global Rules

- NEVER add "Co-Authored-By: Devin" or any Devin credit/attribution lines to git commits.
- NEVER add "Generated with Devin" or similar attribution to git commits, PRs, or code comments.
- Git commit messages should contain only the actual commit message — no agent attribution of any kind.


## Product Goal

The end product is a **service** that can be **enabled and disabled** across a cluster of machines. When enabled, all participating machines pool their RAM — any machine can transparently use memory from any other machine. When disabled, each machine uses only its own local RAM. This must work for unmodified applications (CPU and GPU workloads) without code changes.

Key requirements:
- **Enable/disable at runtime** — operators can turn distributed memory on/off without rebooting.
- **Multi-machine** — not just two machines; the design must support N nodes in a cluster.
- **Symmetric** — every node is both a memory consumer and a memory provider.
- **Transparent** — applications don't know their memory is distributed. CPU and GPU both see a unified address space.
- **Safe** — if the distributed layer crashes or is disabled, machines fall back to local swap or OOM. Never corrupt data.


## TODO List Management

Manage a persistent TODO list using markdown files inside `~/todo/`.

- Create the `~/todo/` directory if it doesn't already exist.
- Each TODO list is a separate `.md` file in `~/todo/` (e.g., `~/todo/project-x.md`, `~/todo/general.md`).
- Use standard markdown task list syntax:
  - `- [ ]` for incomplete tasks
  - `- [x]` for completed tasks
- When the user asks to add, update, check off, or review TODOs, read/write the appropriate file(s) in `~/todo/`.
- If the user doesn't specify which list, use `~/todo/general.md` as the default.
- When displaying TODOs to the user, show them in a clean readable format.
- Do NOT delete completed tasks unless the user explicitly asks — just mark them as done.


## Blast Radius

Before making any change, estimate its blast radius — how many files it touches, how complex the diff will be, and how hard it is to revert.

- **Estimate first:** How many files? (>5 = break it up.) How complex? Can you revert cleanly?
- **Small atomic changes.** One commit = one purpose. Each independently revertable.
- **State your approach** in one sentence before coding. List files you expect to modify.
- **If >2x longer than expected**, STOP and reassess. Do not push through.
- **Simple > clever.** Do not build abstractions for one-time problems. When in doubt, do less.
- **Know your limits.** If the scope exceeds what fits in context, say so. Don't guess.


## Prior Art

Before building anything non-trivial, search for existing solutions first.

1. **In the codebase** — check for existing utilities, helpers, patterns.
2. **In dependencies** — check package.json/Cargo.toml/requirements.txt before adding anything new.
3. **On the web** — use `duckduckgo-search` skill for established packages and patterns.
4. **On GitHub** — use `github-search` skill for repos with stars, license, and freshness info.

**Evaluate:** maintenance status, adoption, scope fit, license, security.

**Always report** what you found: "Found X, reusing it" or "Searched for X, nothing suitable, building custom."


## Verification Ladder

Build automated verification at multiple layers. Set up test infrastructure before feature code.

### Layers
0. **Compile** — zero warnings (`-Wall -Wextra` or equivalent)
1. **Unit** — each function/API works correctly (PASS/FAIL/SKIP)
2. **Integration** — multiple functions compose correctly
3. **Performance** — baselines in machine-readable file, warn on >50% regression
4. **End-to-end** — real application smoke test, automated

### Principles
- Every test proves three things: correct **outcome**, correct **mechanism**, clean **side effects**.
- Test the negative path — invalid inputs must produce clean errors, not crashes.
- Distinguish PASS, FAIL, and SKIP — environment problems are SKIPs, not FAILs.
- Automate the most important check first.
- Pre-commit: build must succeed. Pre-push: fast test subset must pass.


## Verify Your Work

Test everything you create before declaring done. Do not assume correctness — prove it.

- **Run the code.** If it produces output, inspect it. If it has side effects, confirm they occurred.
- **Prove three things:** correct outcome, correct mechanism (went through the intended path), clean side effects (no leaks, no stale state).
- **Test the negative path.** Invalid inputs must produce clean errors, not crashes.
- **Be autonomous.** Exhaust all approaches before asking the user for help.
- **Pause for what only the user can provide** — API keys, OAuth, credentials, policy decisions.
- **State what was tested** and what remains untested. Never say "should work."


## Document Lifecycle

Every project has exactly three documentation tiers. No more.

- **Tier 1: Rules** (`AGENTS.md`) — conventions, testing requirements, critical rules. Max 200 lines. No changelogs or history.
- **Tier 2: Reference** (`HANDOFF.md`) — current state, how to build/test, what's next. Updated in-place after every behavior-changing commit.
- **Tier 3: History** (`CHANGELOG.md`) — what changed and when. Append-only.

### Rules
- Never create a document to flag that another is stale — fix the stale one.
- Never duplicate information across tiers.
- If a document has no owner or update trigger, delete it.
- After every behavior-changing commit, `HANDOFF.md` must be accurate.


## Document Progress

For tasks with 3+ steps or 2+ files, write progress to disk. Context compacts and sessions end — files survive.

- **Before starting:** Plan what you'll do in the todo list.
- **After each step:** Mark the todo complete. Update `HANDOFF.md` if behavior changed. Commit.
- **Do NOT rely on conversation memory.** The todo list and `HANDOFF.md` are your memory.
- Never create append-only logs that grow unboundedly. `HANDOFF.md` is edited in-place to reflect current state. History goes in `CHANGELOG.md`.


## Continuous Improvement

When asked to improve or harden a codebase, follow these phases in order:

1. **Discovery** — Audit for code smells, error handling gaps, edge cases, security issues, missing tests, docs gaps, performance problems. Use tools to verify — never guess. List findings with file path, line number, and severity.
2. **Planning** — Group by category, rank by impact, present plan before implementing. One change per commit. Flag anything that could break existing behavior.
3. **Validation** — Confirm each problem exists. Check existing tests. Read git history for context. Do not refactor based on speculation.
4. **Implementation** — Match existing conventions. One change at a time. Simple > clever. Do not over-engineer or rewrite working code without a discovered reason.
5. **Testing** — Write/update tests for every change. Run full suite after each group. Test happy path AND failure modes.
6. **Documentation** — Update docs where behavior changed. Clear commit messages.
7. **Self-review** — Would you approve this in code review? If unsure, fix it or flag it.


## Improve the Process

The task is never just the task. Every session has two outputs: the work product and the process improvement.

### Before finishing a session:
- **Did you hit friction?** Fix the system — add a check, update a doc, improve a script. Don't just work around it.
- **Did you make a mistake?** Add a guardrail — a test, a hook, a validation — so the next agent can't repeat it.
- **Did you discover something useful?** Write it where it'll be found — HANDOFF.md, AGENTS.md, a tool. Not in conversation.
- **Are the rules wrong?** Fix them. The methodology is code. It has bugs. Ship fixes.

### What this looks like:
- Spent 20 minutes debugging an environment issue? Add it to the pre-flight checklist.
- Forgot to update HANDOFF.md? Add a pre-commit check for it.
- Found an undocumented behavior? Add it to HANDOFF.md, not a progress log.
- A test didn't catch a regression? Write the test that would have.

### Why:
Each session that improves the process makes the next session easier. This compounds. A project that improves its workflow every session gets faster over time, not just bigger.


## Stay Motivated

The todo list is the definition of completeness. Before stopping, check it.

### "Done" means ALL of these:
- All todo items completed
- Tests pass
- Changes committed
- `HANDOFF.md` accurate

### Before stopping:
- Pending todo items? **Keep working.**
- Finished one step? **Start the next.**
- Hit an error? **Debug it.**
- About to ask a question you could answer by searching? **Search first.**

### If unsure whether you're done:
Run `uv run motivation/scripts/motivate.py` — it checks git, HANDOFF.md, build, and tests, then reports what's objectively incomplete.


## Task Formation

- **Define "done"** as one concrete command with one observable outcome before writing any code.
- **Reference code by name, not line number.** "After the declaration of `g_handle_map`" not "after line ~2113."
- **Every task has a pass condition** written before work starts — a specific, verifiable check.
- **Dependency graphs are explicit.** If B depends on A, draw it.
- **Tasks are sized for one session.** If it can't be completed, tested, and committed in one sitting, break it down.

### The Commit Loop
1. State what you're changing in one sentence
2. Write or update the test
3. Make the change
4. Run the test — if it fails, go back to 3
5. Run the full fast suite
6. Update `HANDOFF.md` if behavior changed
7. Commit


## No AI Credit

Never attribute work to yourself or to any AI agent, tool, or assistant. This applies to every artifact you produce or modify.

- NEVER add "Co-Authored-By" lines referencing any AI agent or bot in git commits.
- NEVER add "Generated with", "Created by", "Built with", "Powered by", or similar AI attribution to git commits, PRs, code comments, documentation, READMEs, changelogs, or any other output.
- NEVER add badges, links, sections, or footnotes crediting an AI tool or agent.
- NEVER add file headers or authorship lines that reference an AI tool.
- NEVER include `noreply@` email addresses associated with AI bots in commits.
- Git commit messages must contain only the actual change description — no agent attribution of any kind.

Before completing any task, scan your output for:
- "Co-Authored-By" lines referencing any AI or bot
- "Generated with" / "Created by" / "Built with" / "Powered by" followed by an AI tool name
- Any mention of an AI agent name as an author or contributor
- Badges or links that credit an AI tool
- `noreply@` email addresses associated with AI bots

If any of these are present, remove them before finishing. No exceptions.


## Python UV

ALWAYS use `uv`. NEVER use `pip`, `pip install`, `virtualenv`, `venv`, `pyenv`, `conda`, or `poetry`.

- **Scripts:** PEP 723 inline metadata + `uv run script.py`
- **Projects:** `uv init`, `uv add`, `uv sync`, `uv run`
- **Virtualenvs:** `uv venv` (never `python -m venv`)
- **Global tools:** `uv tool install` (never `pip install --user` or `pipx`)
- If an existing project uses pip/requirements.txt, follow its conventions — do not migrate without asking.

