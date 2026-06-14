# Fork Maintenance Guide

This document describes how to maintain our fork of [jeremychone/rust-genai](https://github.com/jeremychone/rust-genai) with custom OAuth patches while staying in sync with upstream releases.

## Current State (2026-06)

The fork delta has shrunk to **Anthropic OAuth only**. Everything else we once
carried (prompt caching TTL, web search/fetch, UTF-8 chunk buffering, tool-name
hijacking) is now in upstream — see the PR track record below.

| Branch | Upstream base | Delta | Status |
|--------|---------------|-------|--------|
| `eckermann` | v0.5.3 | OAuth + (now-redundant) caching/web/UTF-8 patches | Stable / fallback |
| `eckermann-0.6` | v0.6.5 | **OAuth only** (~110 lines glue + 7 OAuth files) | New, build+unit verified |

**Consumers (e.g. `Eckermann.ink/backend`) should pin a tag, not a moving branch
rev**, for reproducibility:

```toml
genai = { git = "https://github.com/holovskyi/rust-genai", tag = "v0.6.5-eckermann.1" }
```

The 0.6.5 migration required **zero code changes** in the backend — only `Cargo.toml`
(the dep line) and `Cargo.lock`. The public OAuth API was preserved deliberately, the
core genai API is source-compatible in 0.6.5, and the backend's web tools are its own
JSON tools (not genai types). Runtime behavior (esp. `usage` cache-token fields) should
still be confirmed with an OAuth e2e test using a real `sk-ant-oat` token.

### Porting OAuth across a major upstream bump

OAuth is **Claude-Code-CLI-specific and will not be accepted upstream** (see maintainer
preferences below). Treat it as a permanent delta. When upstream restructures the
Anthropic adapter (as 0.6 did, splitting logic into `adapter_shared.rs`), the port is:

1. Branch `eckermann-<ver>` from the upstream tag.
2. Copy the adapter-independent files ~verbatim: `resolver/oauth_{config,credentials,refresh}.rs`
   and `anthropic/oauth_{transform,obfuscate,utils,config}.rs` (they operate on `serde_json::Value`).
3. Re-add the `AuthData::OAuth(OAuthCredentials)` variant **alongside** the upstream variants
   (do not drop `None`/`RequestOverride`/`MultiKeys`).
4. Re-wire `mod.rs` exports (keep upstream's modules; add the OAuth ones).
5. Add the three hooks in `anthropic/adapter_shared.rs`: OAuth detection + Bearer/oauth-beta
   headers + request payload transform in `build_web_request_data`; response transform in
   `build_chat_response`. Plus `proxy_` stripping in `streamer.rs`.
6. `cargo check && cargo test --lib`, then tag `vX.Y.Z-eckermann.N` and point the backend at it.

## Repository Setup

### Remotes

| Remote | URL | Purpose |
|--------|-----|---------|
| `origin` | `https://github.com/holovskyi/rust-genai` | Our fork |
| `upstream` | `https://github.com/jeremychone/rust-genai.git` | Original repository |

### Initial Setup

```bash
# Clone your fork
git clone https://github.com/holovskyi/rust-genai.git
cd rust-genai

# Add upstream remote
git remote add upstream https://github.com/jeremychone/rust-genai.git
git fetch upstream --tags

# Verify remotes
git remote -v
```

## Branch Strategy

```
upstream/main (official releases)
       |
       +-- main (synced with upstream, for PRs)
       |     |
       |     +-- fix/* (bug fix branches for upstream PRs)
       |
       +-- eckermann (OAuth + custom patches)
```

| Branch | Purpose | Push to upstream? |
|--------|---------|-------------------|
| `main` | Synced with upstream, base for PRs | No |
| `eckermann` | Working branch on the 0.5.x line (stable / fallback) | No |
| `eckermann-0.6` | Working branch on the 0.6.x line (OAuth-only delta) | No |
| `fix/*` | Bug fix branches for upstream PRs | Yes (via PR) |

> Once `eckermann-0.6` is validated in production, promote it: rename `eckermann` →
> `eckermann-0.5` (archive) and `eckermann-0.6` → `eckermann` so future syncs land on
> the main working branch again.

## Commit Categories

We maintain two types of changes:

### 1. OAuth/Custom Features (Private)

These stay only in `eckermann` branch:
- OAuth token support (`sk-ant-oat-*`)
- Token refresh functionality
- OAuth request/response transformations
- Custom examples (c13, c14, c15)

**Commit prefix:** `feat(oauth):`, `fix(oauth):`, `^ oauth -`

### 2. Bug Fixes (Contribute to Upstream)

Generic fixes that benefit everyone:
- Bugs in existing Anthropic adapter
- Streaming issues
- Caching problems

**Commit prefix:** `fix(anthropic):`, `fix(streaming):`

## Syncing with Upstream Releases

When a new version (e.g., `v0.5.2`) is released upstream:

### Step 1: Fetch and Check

```bash
git fetch upstream --tags

# See what's new
git log --oneline v0.5.1..v0.5.2

# Check for potential conflicts
git diff --stat v0.5.1..v0.5.2
```

### Step 2: Merge (Recommended Strategy)

We use **merge** (not rebase) because:
- Preserves commit history
- Safer for shared branches
- Easier conflict resolution
- Can be reverted if needed

```bash
git checkout eckermann
git merge v0.5.2 -m "merge: update to upstream v0.5.2"

# Resolve conflicts if any
# Then verify
cargo check
cargo test --lib
```

### Step 3: Push

```bash
git push origin eckermann
```

## Contributing Bug Fixes to Upstream

When you fix a bug that should go to the official repository:

### Step 1: Sync main with upstream

```bash
git fetch upstream
git checkout main
git merge upstream/main
git push origin main
```

### Step 2: Create fix branch

```bash
git checkout -b fix/descriptive-name
```

### Step 3: Cherry-pick bug fix commits

```bash
# Find your bug fix commits in eckermann
git log --oneline eckermann | grep "fix(anthropic)"

# Cherry-pick them
git cherry-pick <commit-hash>
```

### Step 4: Verify

```bash
# Check no OAuth dependencies leaked in
git diff main..fix/descriptive-name | grep -i oauth

# Verify compilation
cargo check
cargo test --lib
```

### Step 5: Create PR

```bash
git push origin fix/descriptive-name

# Create PR via gh CLI
gh pr create --repo jeremychone/rust-genai \
  --base main \
  --head holovskyi:fix/descriptive-name \
  --title "fix(anthropic): description" \
  --body "Description of the fix"
```

### Step 6: After PR is merged

```bash
# Update main
git fetch upstream
git checkout main
git merge upstream/main
git push origin main

# Delete fix branch
git branch -d fix/descriptive-name
git push origin --delete fix/descriptive-name

# Your eckermann already has these changes, no action needed
```

## Using in Projects

### Via Tag (recommended)

Pin a **tag**, not a branch — tags are immutable, so the build is reproducible and
`cargo` caching stays predictable. A branch rev moves under you; a tag never does.

```toml
[dependencies]
genai = { git = "https://github.com/holovskyi/rust-genai", tag = "v0.6.5-eckermann.1" }
```

### Via Local Path (for development)

```toml
[dependencies]
genai = { path = "../rust-genai" }
```

## Releasing & Tagging

**Tags are append-only. Never move a tag.** Each released change gets a new tag; the
consumer bumps one line to adopt it (and reverts that one line to roll back).

### Versioning scheme

`v<upstream>-eckermann.<N>`:

| Tag | Meaning |
|-----|---------|
| `v0.6.5-eckermann.1` | upstream 0.6.5, our iteration #1 |
| `v0.6.5-eckermann.2` | same upstream 0.6.5, our next change (OAuth fix, etc.) |
| `v0.6.6-eckermann.1` | pulled upstream 0.6.6, our iteration #1 |
| `v0.7.0-eckermann.1` | migrated to 0.7 |

Rule: **our code changes on the same base → bump `.N`**; **new upstream base → change
`<upstream>` and reset `.N` to 1**.

### Cutting a release

```bash
# 1. Commit changes on the working branch (e.g. eckermann-0.6)
git add -A && git commit -m "fix(oauth): ..."
git push origin eckermann-0.6

# 2. New immutable tag (increment the suffix)
git tag -a v0.6.5-eckermann.2 -m "describe the change"
git push origin v0.6.5-eckermann.2

# 3. In the consumer (e.g. Eckermann.ink/backend): bump the one line
#    Cargo.toml:  tag = "v0.6.5-eckermann.2"
cargo update -p genai
cargo check --locked
```

### Why never move a tag

- `cargo` caches git deps by tag; a moved tag can silently build stale code or pull new
  code unexpectedly.
- A moved tag breaks reproducibility — the same `Cargo.lock` would point at different code.
- Rollback is trivial when tags are stable: put the previous tag back in `Cargo.toml`.

> Renaming branches (e.g. promoting `eckermann-0.6` → `eckermann`) does **not** affect
> consumers, because they pin a tag, and tags don't move when branches are renamed.

## Quick Reference

| Task | Command |
|------|---------|
| Fetch upstream | `git fetch upstream --tags` |
| List upstream tags | `git tag -l 'v*'` |
| Update eckermann to v0.5.2 | `git checkout eckermann && git merge v0.5.2` |
| Sync main with upstream | `git checkout main && git merge upstream/main` |
| Check OAuth code isolation | `git diff main..fix/xxx \| grep -i oauth` |
| Test cherry-pick | `git cherry-pick --no-commit <hash> && git reset --hard` |

## Troubleshooting

### Conflict During Merge

```bash
# See conflicting files
git status

# After resolving conflicts
git add <resolved-files>
git commit

# To abort if needed
git merge --abort
```

### Cherry-pick Conflicts

```bash
# If cherry-pick fails
git cherry-pick --abort

# Manual approach: create patch and apply
git show <commit> > fix.patch
git apply --3way fix.patch
```

### Checking What Patches We Have

```bash
# See all our commits on top of upstream tag
git log --oneline v0.5.2..eckermann

# See only OAuth-related
git log --oneline v0.5.2..eckermann | grep -i oauth

# See only bug fixes
git log --oneline v0.5.2..eckermann | grep "fix(anthropic)"
```

## Example: Full Update Cycle

```bash
# 1. New upstream release v0.5.3 is out
git fetch upstream --tags

# 2. Update eckermann
git checkout eckermann
git merge v0.5.3 -m "merge: update to upstream v0.5.3"
cargo check
git push origin eckermann

# 3. If we have bug fixes to contribute
git checkout main
git merge upstream/main
git push origin main

git checkout -b fix/some-bug
git cherry-pick abc123
cargo check
git push origin fix/some-bug
gh pr create --repo jeremychone/rust-genai ...
```

## Upstream Maintainer Preferences (from PR feedback)

Lessons learned from contributing to `jeremychone/rust-genai`. Follow these to
maximize the chance a PR is accepted.

1. **Separate fixes from API changes — one concern per PR.**
   Bug fixes can be applied to the *current* release line (e.g. `0.5.x`), while
   API changes are batched into the next major (e.g. `0.6.x`, which takes longer
   because the maintainer combines several API changes into one release). Mixing
   both in a single PR forces the whole thing to wait for the major release.
   > "It's good to separate fixes vs. API changes so that I can apply the changes
   > to the current release, while API changes come after."

2. **Avoid provider-specific types and variant proliferation.**
   The maintainer dislikes adding many Anthropic-specific types/enum variants.
   Designs that *normalize* behavior across providers (Anthropic, OpenAI, Gemini)
   are strongly preferred. Before proposing typed variants, check how the same
   feature would map onto the other providers.
   > "There are so many types and variants specific to Anthropic... If we start
   > having variants and types per provider category, this will explode."
   The generic escape hatch the maintainer favors: `Custom` variants on
   `ToolName`, `ToolConfig`, and `ContentPart` so callers can pass/read data the
   library doesn't explicitly type.

3. **A good idea may be reimplemented rather than merged.**
   Even a closed PR can shape upstream. Our web-tools PR (#133) was closed but the
   maintainer re-implemented it "in a different, more generic way," explicitly
   "strongly inspired by" it. Expect influence, not necessarily your code.

### Our PR track record

| PR | Topic | Outcome | Notes |
|----|-------|---------|-------|
| #130 | prompt caching fixes | **Merged** | Released in `0.6.0-alpha.1`. Our code. Maintainer later extended TTL (`Ephemeral1h/24h`) himself. |
| #133 | web search + web fetch | Closed | Reimplemented generically upstream (`ToolName::WebSearch` + `WebSearchConfig` + `Custom`). Our idea, not our code — so the 0.6 API shape differs from our fork. |
| #134 | tool hijacking + streamer state | Closed | Solved independently via the `ToolName` redesign. |
| #245 | mid-stream `error` event | Open | Pure fix, no API change, no OAuth — the kind of PR the maintainer can take into the current line. |

Note: our UTF-8 chunk-boundary fix (`utf8_carry`) was **never** submitted as a PR;
upstream implemented the same thing independently. It already exists in
`upstream/main`, so do not open a PR for it.

### Implication for our OAuth patches

Our Anthropic OAuth support (`sk-ant-oat` tokens + auto-refresh + request/response
transform + obfuscation) is Claude-Code-CLI-specific and provider-specific by
nature, so it is unlikely to be accepted upstream given preference #2. Treat it as
a **permanent fork delta** to carry across upstream updates, not as something to
upstream.
