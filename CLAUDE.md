# bcachefs-tools

bcachefs-tools is the userspace component of bcachefs, a modern
copy-on-write filesystem for Linux. It provides formatting, mounting,
fsck, and administrative commands. The codebase is a mix of Rust (command
dispatch, argument parsing, most user-facing logic) and C (kernel-internal
operations that use kernel data structures directly, plus the FUSE mount
command).

The primary maintainer is Kent Overstreet.

## Long-term direction

The goal is to convert the entire bcachefs codebase — both userspace
tools and the kernel filesystem — to Rust. bcachefs-tools is mostly
converted already; the kernel codebase is next.

**Why convert everything:** The intermediate state of a mixed Rust/C
codebase has significant overhead: FFI glue, duplicated types, inability
to use Rust's error handling across the boundary. Converting as one big
project (rather than incrementally) avoids spending months maintaining
intermediate glue that will disappear once the conversion is complete.

**What we gain:**
- Result-based error handling makes error paths explicit and
  machine-checkable, reducing bugs in the field
- Ownership and lifetime enforcement eliminates classes of memory safety
  bugs at compile time
- The codebase becomes easier to work on and refactor for both humans
  and AI
- Most importantly: a fully-Rust codebase is the stepping stone to
  **formal verification**. A fully verified general-purpose filesystem
  has never existed — Rust's type system makes it an achievable goal.

**Conversion approach:** Get code into Rust with minimal semantic
changes first, then polish thoroughly — keep going on interfaces and
idioms until there's nothing more to improve. Take thorough notes
during conversion — not just TODOs but analytical frameworks: safety
properties, invariants, error handling patterns, ownership semantics.
These notes are groundwork for subsequent verification work.

**Autonomous work:** This long-term direction is context for independent
action. When reading code for any reason, look for opportunities to
advance these goals — especially note-taking. If you've just read a
large chunk of code and can see what the plan for formal verification
would look like, or what invariants a subsystem maintains, or where the
safety boundaries are: write it down. Those observations are hard to
reconstruct later.

## Working approach

### Research first, implement second

Filesystem code demands a higher standard of correctness than typical
software development. When approaching a new problem:

1. **Research thoroughly.** Read the relevant code, understand the
   existing patterns, explore all the options. Write up findings.
2. **Design collaboratively.** Bring analysis back for discussion before
   committing to an approach, especially for anything touching kernel
   code or key interfaces.
3. **Implement incrementally.** Break work into small, reviewable pieces
   rather than planning and implementing entire features in one pass.

Research and analysis notes are a first-class output, not just
scaffolding for implementation. They should be dated and checked into the
repository under `.claude/` so that future developers (human and AI) can
see what's been analyzed and what was decided.

### Calibrate to the risk

- **Kernel code** (`libbcachefs/`, `fs/bcachefs/`): Go slow. Research
  thoroughly. Always discuss design before implementing. Pay special
  attention to error paths, transaction restart handling, and locking.
  Don't commit kernel C changes to the tools tree — kernel code is
  synced separately.

- **Tools leaf code** (Rust commands, argument parsing, display logic):
  More autonomy is appropriate here. Use good judgment, make clean
  commits, and keep things idiomatic.

- **Interfaces and abstractions** (bch_bindgen wrappers, safe Rust APIs
  over unsafe C): These are in between. The design matters because it
  affects everything built on top. Discuss the shape before building.

## Code standards

### Correctness

This is filesystem code — the highest standards for correctness apply.
Always research and/or ask if you're not sure. Special care must be taken
with:
- Error paths, including errors indicating filesystem consistency problems
- Transaction restart handling (see memory notes for detailed patterns)
- Locking and RCU patterns
- Permissions checks

### Error messages

Error messages must be clear and thorough while being concise. We often
debug in the field, so messages should include:
- What happened
- The operational context (what we were doing)
- Relevant state information
- What corrective action was taken (if any)

### Documentation

Prefer good naming and structure over inline comments. Short comments
to break files into sections are fine; long inline explanations usually
mean the code should be restructured to be self-explanatory.

Each file should have a notes section at the top describing:
- What the file is for (purpose and scope)
- Key architectural decisions and design rationale
- Important invariants or non-obvious properties
- Relationships to other subsystems

This is also the right place for analytical notes — safety properties,
ownership semantics, invariants, things to revisit later. These notes
document the *thinking* behind the code, not just what it does.

### Code style

- Review your code before showing it. Simplify. Keep patches clean and
  orthogonal.
- Check for extraneous leftovers in diffs.
- Don't add unnecessary wrappers or abstractions — if something only has
  one caller, inline it.
- Don't over-engineer. Only make changes that are directly requested or
  clearly necessary.
- Use `out` (not `buf`) for printbuf parameters.
- Use x-macros for enums that need string arrays.
- Use `printbuf_tabstop_push()` for aligned output in to_text functions.

### Commits

Make commits that make sense without asking — clean, orthogonal commits
are a good thing. Commit working code before refactoring to keep diffs
clean and make it easy to compare old vs new or revert.

## Architecture

- **Rust**: Command dispatch (`src/bcachefs.rs`), all command
  implementations (`src/commands/`), wrappers over C APIs
  (`src/wrappers/`, `bch_bindgen/`)
- **C shims** (`c_src/rust_shims.c`): Thin wrappers around kernel macros
  and iteration patterns that can't be expressed through bindgen
  (LE64_BITMASK setters, `for_each_member_device`, btree node walking,
  crypto operations)
- **C commands**: Only `cmd_fusemount.c` remains as a pure C command.
  `cmd_image.c` and `cmd_migrate.c` have Rust arg parsing but C cores.
- `bch_bindgen` generates Rust bindings via bindgen, plus hand-written
  safe wrappers for btree iteration, journal parsing, extent iteration,
  printbuf, and superblock access
- Typed bkey dispatch (`BkeyValSC`, `BkeyValI`) is code-generated from
  the `KEY_TYPE_*` x-macro
- Transaction restart handling uses `lockrestart_do` / `for_each`
  patterns that retry on restart errors

## Reference

- [doc/build.md](doc/build.md) — build commands and tips
- [doc/testing.md](doc/testing.md) — ktest usage
- [doc/x-macro-mechanism.md](doc/x-macro-mechanism.md) — importing C x-macros into Rust
