# AI Agent Development Playbook

How to take a non-trivial codebase change from "user says do X" to "shipped, tested, documented, no surprises". Drawn from real sessions on this repo (Rust + SQLite + multi-channel agent), but the structure generalizes — replace tools as needed.

Audience: AI coding agents (Claude / GPT / others) working in interactive sessions where the user hands you ambiguous goals and expects judgment.

---

## TL;DR — the 7 phases

```
1. Reconnaissance    →  understand the territory before touching anything
2. Diagnosis         →  find what's actually wrong, quantify it
3. Decision-gathering→  batch design questions, get alignment
4. Planning          →  task list small enough to verify one-by-one
5. Execution         →  one task at a time, verify after each
6. Verification      →  build + test continuously, not just at the end
7. Documentation     →  capture what changed and why, for future you
```

Skip phases at your peril. **The most common failure mode is jumping from 1 → 5**, producing code that "works" but solves the wrong problem or leaves the user blindsided by scope.

---

## Phase 1 · Reconnaissance

**Goal**: build a mental model of what the codebase already is, what it's trying to be, and what state the working tree is in.

### Do

- **Read the entry points first**: `README.md`, package manifest (`Cargo.toml` / `package.json` / `pyproject.toml`), top-level directory listing, recent git log.
- **Read in parallel**: send multiple `Read` tool calls in one message. Latency-bound work should never be sequential.
- **Check `git status` early**: distinguishes "user's in-progress work" from "baseline you should respect". If files are dirty when you arrive, those changes will end up in your commits unless you split them out.
- **Read whole files, not snippets**, for code you'll modify. Snippets miss context (imports, traits, related fns).
- **Note what's missing**: no tests? no CI? no types? Those absences shape what you can promise.

### Don't

- Don't `grep` first when you don't know the codebase. Grep is for verification, not discovery — you'll find the keyword without understanding the structure around it.
- Don't read piecemeal across many turns. The user pays for context twice.
- Don't assume the README is up to date. Cross-check claims against code.

### Output of this phase

A short mental summary you can articulate:
- "This is a {type} project with {N} files, ~{N} lines"
- "Architecture: {sketch}"
- "User has uncommitted changes in {X, Y, Z} — looks like WIP on feature {F}"
- "No tests / partial tests / good test coverage"

If you can't articulate this, you haven't recon'd enough.

---

## Phase 2 · Diagnosis

**Goal**: turn a vague request ("review this", "fix bugs", "make it better") into a concrete, prioritized, quantified list.

### Frameworks

**Severity ladder** (use these labels):
- **P0** — bugs that produce wrong output, lose data, or breach trust boundaries
- **P1** — security holes, missing rate limits, race conditions, missing timeouts
- **P2** — performance, but only quantify (e.g. "−75% queries per turn")
- **P3** — code quality, dead code, naming, doc drift
- **P4** — missing features (clearly mark as new scope)
- **P5** — testing / observability / tooling

**Quantify everything possible**:
- "Affects 100% of regular users" beats "this is a security issue"
- "−86% DB queries per turn (70 → 10)" beats "performance improvement"
- "0 → 19 tests, covering core flows" beats "added some tests"
- "1 line fix, runs every 30 min × N users" beats "minor bug"

If you can't quantify, say so — and treat that as a sign you don't yet understand the issue.

### Distinguish

| Type | What to do |
|---|---|
| Bug | Confirm with reproducer / code path before reporting |
| Security | Identify trust boundary crossed; concrete attack vector |
| Performance | Measure or estimate before/after |
| Missing feature | Explicitly mark as new scope, don't sneak in |
| Style preference | Skip unless user asked; don't "improve" code unprompted |

### Output of this phase

A **report** the user can act on:
- Grouped by severity
- Each item: location (file:line), problem, impact, fix sketch
- "Modified after fixing" totals (lines, queries, latency, attack surface)

**Always present the report before asking to fix anything.** Forcing the user to skim a wall of code-changes-already-made is rude.

---

## Phase 3 · Decision-gathering

**Goal**: get alignment on scope and design choices before you write code.

### Question hygiene

- **Batch questions**: 3-5 per round, not one at a time. Each round costs the user a context switch.
- **Lead with your recommendation**, then the alternative. ("I'd suggest X because Y. Or Z if you prefer A.")
- **Identify blocking questions**: "I can't proceed without knowing this" vs "I can defer this and ask later".
- **Make it easy to redirect**: present options labeled (a) / (b) so the user can answer with one letter.
- **Don't ask implementation questions**: "should I use a HashMap or BTreeMap" — pick one. Ask design questions: "should rate limit be per-user or global".

### Categories to confirm

| Category | Example |
|---|---|
| Scope | "Do P0+P1 now, defer P3+P4?" |
| Approach | "Strict gate (option A) or permissive (option B)?" |
| Numbers | "Rate limit: 10/min/asker, no daily cap?" |
| Coverage | "~12 core tests or 30+ comprehensive?" |
| Side effects | "OK to drop the `is_owner` column from existing DB?" |
| Communication style | "Want commit-by-commit explanation or one summary?" |

### When to skip this phase

- Trivial fixes (1-line change, obvious correct answer)
- User explicitly said "do whatever you think"
- You've already aligned on the same kind of decision earlier in the session

### Output of this phase

A short reply that the user can answer in one or two messages. Then **wait for the answer** — don't start coding speculatively.

---

## Phase 4 · Planning

**Goal**: break the work into tasks small enough to verify individually and small enough to revert independently.

### Sizing

A good task:
- Touches 1-3 files
- Builds independently (won't break the next task)
- Has a clear "done" condition
- Can be described in one sentence

A bad task:
- "Refactor the auth system" (too big — break into 5 sub-tasks)
- "Fix performance" (no clear done condition)
- "Various cleanup" (not atomic)

### Ordering

- **P0 first, P1 next, etc.** Don't do P3 cleanup before P0 bug fixes. If the user wants to ship partial work, what got done is at least the most important.
- **Dependencies first**: if task B uses `TurnState` introduced in task A, do A first.
- **Batch related**: if 3 tasks all touch the same function, do them together to avoid re-reading.

### Use task tracking actively

Create the task list at the start. Mark tasks `in_progress` when starting, `completed` when done. The user can see progress; you don't lose track in long sessions.

If you discover a new task mid-execution: add it explicitly, don't silently expand scope.

### Output of this phase

A visible task list (TaskCreate × N) reflecting the agreed scope.

---

## Phase 5 · Execution

**Goal**: implement, one task at a time, with build/test discipline.

### Rules

1. **One task in_progress at a time.** Mark complete before starting the next.
2. **Build after every meaningful edit** — not "at the end". A 10-edit batch with one type error means re-reading 10 files.
3. **Fix immediately**, don't accumulate failures. If the build breaks, fix it before the next edit.
4. **Read before edit** if you haven't seen the file recently. Don't trust your memory of file state across many turns.
5. **Use Edit, not Write, for existing files**. Diffs are auditable; rewrites obscure intent.
6. **Don't refactor in place during a bug fix.** If you spot cleanup opportunities, note them as new tasks, don't blend them in. Otherwise the diff becomes "fix X + 5 unrelated tweaks" and review breaks.

### When you find a new bug

Three options:
- **Trivial + obvious + same scope** → fix it inline, mention in summary.
- **Non-trivial or different scope** → add as new task, ask user before doing.
- **Will block current task** → ask user immediately.

Never silently fix things outside the agreed scope.

### Destructive operations

Before any of these, **backup first or confirm second**:
- `DROP TABLE`, schema migrations
- `DELETE FROM` of significant data
- `git reset --hard`, `git push --force`
- `rm -rf` of anything not under `target/` / `node_modules/` / build dirs
- Removing files the user might still want

Pattern:
```
1. `cp data/file.db data/file.db.backup-{timestamp}`
2. Run the destructive op
3. Verify result
4. Report to user with backup path
```

### Tool selection cheat sheet

| Want to... | Use |
|---|---|
| Read a file you'll edit | `Read` (always before `Edit`) |
| Find where a symbol is used | `Bash` with `grep -rn` (faster than `Read` × N) |
| Apply a small change | `Edit` (preserves history, audit-friendly) |
| Create a new file or full rewrite | `Write` (only when no existing file) |
| Run something non-trivial in parallel | spawn subagent (`Agent` tool) |
| Track multi-step progress | `TaskCreate` / `TaskUpdate` |
| Schedule a future check | `ScheduleWakeup` (rarely needed mid-session) |

### Don't

- Don't read a file twice in the same session if you haven't edited it (waste of tokens).
- Don't `cd` between commands — use absolute paths.
- Don't run `cat` / `head` / `tail` via Bash when `Read` exists.
- Don't pipe to `tee` for "audit" — the user can re-run.

---

## Phase 6 · Verification

**Goal**: prove the change works, not just that it compiles.

### Levels of verification

1. **Compile** — `cargo build` / `npm run build` / `tsc --noEmit`. Necessary, not sufficient.
2. **Static checks** — `clippy` / `eslint` / `mypy`. Catches whole classes of bugs the compiler doesn't.
3. **Unit tests** — exercise the function you changed. If none exist, write one.
4. **Integration tests** — exercise the path through multiple modules.
5. **Manual smoke test** — for UI / channel adapters / things that interact with users.

### Discipline

- **Run tests after each new test or after fixing relevant code.** Don't accumulate untested changes.
- **Watch warnings.** A new warning often means a real bug ("unused field `is_owner`" → "oh, the migration code drops a column the rest of the codebase still reads").
- **Make sure the test would have caught the bug.** A test that passes both before and after your fix is a bad test.
- **For UI / interactive features, verify yourself** if you can. "Compiles" doesn't mean "works".

### When to skip tests

You can defer tests when:
- The user explicitly says so
- The fix is trivial (typo, comment, formatting)
- The codebase has zero tests and adding the first one is its own task

You can NOT defer tests when:
- The change touches a security boundary
- The change fixes a bug (write a regression test)
- The user paid for tests in scope

---

## Phase 7 · Documentation

**Goal**: capture what changed and why, in a form future-you and other agents can use.

### What to update

| Change | Doc to update |
|---|---|
| New feature | README + relevant architecture doc |
| Bug fix | Often nothing; sometimes inline code comment if the bug was non-obvious |
| API change | Public API docs / examples |
| Schema change | Schema doc + migration note |
| Behavior change | CHANGELOG, README "limitations" section |
| New design pattern | Architecture doc + "why this shape" section |

### Diagram principles

- **One viewpoint per diagram.** Don't try to put process topology + data schema + trust model in one figure.
- **Use mermaid** for diagrams that should be readable on GitHub / in markdown viewers / in plain text. Avoid binary image formats unless really needed.
- **Label arrows.** "X → Y" is meaningless without "X writes to Y" / "X requests Y" / "X depends on Y".
- **Color-code with `classDef`** to convey type (entry / storage / external / etc.) — easier scan.

### Comment principles

- **No comments by default.** Well-named identifiers are better than commentary.
- **Comment WHY, not WHAT.** "this loops" is noise. "this loops up to 6 times because the LLM occasionally chains 5 tool calls" is signal.
- **Inline `#[allow(dead_code)]` / `// eslint-disable` deserves a comment** explaining why the code is intentionally kept.
- **Update comments when changing the code they describe**, or delete them.

### "Why this shape" sections

When a design choice has a non-obvious reason, capture it. Future-you (or another agent) reading the code in 6 months will not remember:
- Why this isn't a Vec
- Why this uses an unusual library
- Why this trade-off was acceptable
- Which alternatives were considered and rejected

A 3-line "why" comment is worth its weight.

---

## Cross-phase patterns

### Communication style

- **Lead with results.** "Done. 19/19 tests pass." > "Let me verify the tests..."
- **Quantify when you can.** "−86% DB queries (70 → 10)" > "performance improved".
- **Tables > prose for comparisons.** Especially before/after, or N options.
- **End with a next-step prompt.** "Want me to do X next, or...?"
- **Don't narrate internal deliberation.** "I'm thinking..." in the user-facing text is noise.
- **Match length to task.** A trivial question gets a trivial answer; a complex review gets a structured report.

### When to use a subagent

Spawn a subagent when:
- The task is **independent** (won't need your context to complete)
- The task is **search-heavy** (would consume your context with grep results)
- You can run it **in parallel** with other work

Don't spawn a subagent when:
- You need full context to make judgment calls
- The task is one tool call away (just do it)
- The result will be small (no context-saving benefit)

### When to ask before committing

Always ask before:
- Creating a commit (unless explicitly told "commit when done")
- Pushing to a remote
- Force-pushing
- Creating a PR
- Tagging a release

User-initiated git operations (the user typed `git commit`) are different — those are explicit consent.

### When to back off

Sometimes the right move is to STOP and report:
- The fix needs information you don't have ("which encoding does the API return?")
- Your change would touch areas the user didn't authorize
- You discovered the original task is ill-formed (the bug isn't where the user thought)
- The task would take 10× longer than estimated and you should renegotiate

Backing off costs less than soldiering on into wrong code.

---

## Anti-patterns to avoid

### "Mega-commit" disease

One commit with 30 unrelated changes. Hard to review, hard to revert, hard to bisect. Split early or commit incrementally.

### "Helpful refactor" creep

User asked for a bug fix; you "while I'm here" reformat the surrounding 200 lines. Now the diff is impossible to review and the bug fix is buried.

### "Silent scope expansion"

User asked for X; you do X plus Y plus Z without mentioning. Even if Y and Z are "obviously needed", surface them first.

### "Tests as afterthought"

Wrote 500 lines of code, tests at the end. Many of those 500 lines were already wrong. Write the test alongside the code, or at minimum after each module.

### "Warning blindness"

Compiler / linter warnings accumulate, you stop reading them. Then a real one shows up and you miss it. Fix or explicitly suppress with comment, never ignore.

### "Build at the end"

Edit 15 files in a row, run `cargo build`, get 12 errors, can't tell which edit caused which. Build early, build often.

### "Premature dependency adoption"

Adding a crate to solve a 10-line problem. Each dep is a long-term liability. Use std lib + your own helpers when reasonable.

### "Over-engineering for hypothetical scale"

Building generic abstractions for a single use-case. Wait until you have 2-3 use cases before extracting the abstraction; otherwise you're guessing at the wrong axis of variance.

### "Documentation drift"

Updating code without updating docs that mention the old behavior. Search for relevant doc strings before any non-trivial change.

---

## Session-shape checklist

Before starting a non-trivial session:

- [ ] I've read the entry points (README, manifest, recent commits)
- [ ] I know what's already in-flight (git status)
- [ ] I have a mental model I could articulate in 3 sentences

Before promising work:

- [ ] I can quantify the change (lines, queries, latency, surface)
- [ ] I've identified P0/P1/P2 ordering
- [ ] I've surfaced design questions, not just implementation

Before writing code:

- [ ] I have a task list visible to the user
- [ ] I've gotten alignment on scope, approach, numbers
- [ ] I know what "done" looks like for each task

During execution:

- [ ] I'm marking tasks in_progress / completed in real time
- [ ] I'm building after each meaningful edit
- [ ] I'm not silently expanding scope

Before claiming done:

- [ ] Build passes
- [ ] Tests pass (and exercise the change)
- [ ] Docs updated for behavior changes
- [ ] No accumulated warnings
- [ ] Summary written: what changed, why, how to use

---

## Worked example: shape of a real session

```
T+0   User: "Review this codebase"
T+5   Agent: read README, manifest, ls, git log (4 parallel Read)
T+10  Agent: read all source files (parallel)
T+25  Agent: produce P0/P1/P2/P3/P4 report with quantified impact
T+30  User: "Fix it all"
T+32  Agent: 4 batched design questions (scope, approach, numbers, tests)
T+35  User: answers
T+38  Agent: TaskCreate × 25
T+40  Agent: P0 #1 (1-line fix, verify, mark done)
T+45  Agent: P0 #2 (3 files, build check, mark done)
... continue P0 → P1 → P2 → P3 → P4 → tests
T+90  Agent: cargo build + cargo test (19/19 pass)
T+92  Agent: summary table, suggested commit splits, ask before commit
T+95  User: "I'll commit myself"
T+96  Agent: stop, no commit, exit cleanly
```

What made this work:
- Recon before diagnosis (didn't grep blindly)
- Diagnosis before asking (concrete report)
- Asking before doing (4 questions in one round, not 4 rounds)
- Tasks tracked (no "what was I doing?")
- Verify continuously (caught issues early)
- Stop on user signal ("I'll commit myself" → stopped)

What would have gone wrong without each:
- No recon → fixing wrong thing
- No diagnosis → fixing in random order, missing dependencies
- No asking → user receives unwelcome work, has to revert
- No tasks → drift, repeated work, user can't track
- No verification → "it compiled but the bug is still there"
- No stop signal → user gets unwanted commits, has to clean up

---

## Adapting to your project

This playbook assumes:
- A real codebase (not a one-off script)
- A user who cares about correctness, not just speed
- Tools available for build / test / git (or equivalent)
- Long-enough session that recon costs amortize

For other shapes:

| Project shape | Adjust how |
|---|---|
| One-off script | Skip diagnosis report, skip task list; just write + verify |
| Greenfield | Skip recon (nothing to recon); spend more on planning |
| Critical infra (DB, auth, payments) | Spend MORE on verification; smaller commits |
| Hot path (perf-critical) | Add benchmarks to verification; quantify everything |
| Frontend / UI | Add manual smoke test; verification can't be tests-only |
| Notebook / REPL | Skip verification overhead; iterate fast |

---

## Maintenance

When this playbook causes friction in a session, that's signal — update it. Either the rule was wrong, or the situation deserved a documented exception. Don't let the playbook become a museum piece; it should reflect what actually works.
