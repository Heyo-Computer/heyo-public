# Printer
In the "Bobiverse", the printer is the technology that enables the Bobs' self replicating journey across the stars. The printer works at the atomic level and can produce any good, including more printers and the equipment to replicate the Bobs themselves. In this future, humans trade time on the printers as currency. They are the foundation of the universe's economic and scientific ambitions.

LLMs are good at writing code. Actually, that's probably what they are best at. Integrating "tools" and running in a loop creates the powerful "agent" paradigm. One more layer of abstraction is the "code factory" which uses a system of agents to produce software autonomously from a specification. Now, humans can produce a lot of software by managing an agent and poking it when it needs to keep going; this commands a lot of attention from a human and ultimately becomes a bottleneck in agentic development and the code factory pattern tries to solve for that particular bottleneck by allowing the human to draft requirements and then startup the factory before moving on to another task or factory. There are complications of course; a system of agents has a lot of moving parts and can burn tokens at an exorbitant rate. 

`printer` aims to implement the simplest form of the code factory pattern. This isn't Gastown so don't expect a dozen subagents running at once. Progress and memory are file based for durability. The plugin system provides a simple way to extend the printer with existing CLI tools. Multi-agent support lets you configure the models and agents in use. 

## Components
The project is made up of 3 CLIs, agent skills, and a plugin system for extending functionality with other tooling. 

### Printer
The core CLI that manages agent sessions to program against a spec file. 

### Computer
CLI that implements programatic desktop interactions for Wayland. Allows agents to work with the desktop directly (on Linux).

### Codegraph
Uses tree-sitter to parse, graph, and query a codebase. Additionally contains patch commands for applying diffs to files. 

The CLI works with skills to help keep token usage efficient and reduce redundant searching and reading of files. 

### Printer Plugins
The plugin system allows the printer to be extended by running arbitrary commands or skills with lifecycle hooks. See [Hooks](printer/HOOKS.md)

### Sandbox (heyvm)
Every `printer exec` can dispatch each agent turn through an isolated
[heyvm](https://docs.heyo.computer) worktree instead of running on the host.
Install and configure via the bundled plugin — see
[plugins/heyvm/README.md](plugins/heyvm/README.md) for the two-step install
and the per-exec lifecycle (sync in → run+review inside the worktree →
sync out → destroy).

**UI/web review on the host.** The heyvm sandbox is a headless microVM with no
Wayland/X11 display, so the `computer` tool cannot click-test inside it. When a
standalone `printer review` detects that the diff touches a UI/web surface
(`.tsx/.jsx/.vue/.svelte/.html/.css/...`, or `.ts/.js` under `web/`,
`frontend/`, `ui/`, `client/`, `src/components/`, `app/`) **and** the host has a
real display (`$WAYLAND_DISPLAY`/`$XDG_SESSION_TYPE` set and `/dev/uinput`
present), it automatically runs review on the host (no sandbox) so the UI can be
exercised. Pass `--no-ui-host` to force the sandbox anyway. Because `printer
exec` shares one sandbox across run + review, UI review under `exec` is not
auto-routed — run `printer exec ... --no-sandbox` (or a separate `printer
review`) when you need the `computer` tool to click-test.

### Agent Plugins
the Printer CLI can install a plugin for Claude and OpenCode agents to utilize `codegraph` for searching and patching files. 

### Skills
Skills are made available to the agents during run and review.
```bash
npx skills add heyo-computer/printer
```

## Getting Started

### Prerequisites

- **Rust toolchain** — `rustc` and `cargo` (install via [rustup](https://rustup.rs))
- **An agent CLI** — at least one of:
  - [OpenCode](https://opencode.ai) — `opencode` on `$PATH`
  - [Claude Code](https://docs.anthropic.com/en/docs/claude-code/overview) — `claude` on `$PATH`

### 1. Build and install CLIs

```sh
make install        # builds printer, computer, codegraph in release and installs to ~/.local/bin
```

Or build individually:

```sh
make install-printer     # just the core orchestrator
make install-codegraph   # just the tree-sitter code query tool
make install-computer    # just the Wayland desktop automation tool (Linux)
```

Verify:

```sh
printer --help
codegraph --help
```

### 2. Install printer plugins

Plugins add lifecycle hooks, skills, sandbox drivers, and agent integrations.

```sh
# Core plugins (recommended for all users)
printer add-plugin path:plugins/acp-runtime          # shared ACP runtime skill (required for ACP agents)
printer add-plugin path:plugins/codegraph            # codegraph lifecycle hooks

# OpenCode ACP agent integration
printer add-plugin path:plugins/opencode             # launches `opencode acp` for persistent sessions
printer add-plugin path:plugins/codegraph-opencode   # codegraph agent + /cg-* slash commands for opencode

# Claude Code ACP agent integration (if using Claude)
# printer add-plugin path:plugins/codegraph-claude

# Optional: sandbox isolation via heyvm
# printer add-plugin path:plugins/heyvm

# Optional: Wayland desktop automation (Linux)
# printer add-plugin path:plugins/computer
```

Verify installed plugins:

```sh
printer plugins
```

### 3. Install the opencode CLI (if using OpenCode)

The `opencode` binary must be on your `$PATH`. Install from the official repo:

```sh
# See https://github.com/sst/opencode for the latest install method
# Common approaches:
go install github.com/sst/opencode@latest
# or via npm:
npm install -g opencode
```

Configure your AI provider:

```sh
opencode auth login    # interactive provider setup
# or set env vars like ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.
```

### 4. Set up opencode for a project

After cloning this repo (or any repo with the codegraph-opencode plugin):

```sh
# Copy the agent definition and slash commands into your project
mkdir -p .opencode
cp -r plugins/codegraph-opencode/agent   .opencode/
cp -r plugins/codegraph-opencode/command .opencode/

# Create or merge the project config
# If you already have an opencode.json, merge the `instructions` and `tools` blocks
cp plugins/codegraph-opencode/opencode.json .   # or merge manually
```

Initialize the codegraph index:

```sh
codegraph index    # one-time; printer exec auto-spawns a watch daemon
```

### 5. Initialize a project spec

```sh
printer init                    # writes ./spec.md
printer init plans/auth.md      # writes to a specific path

# In an already-initialized project (creates auto-numbered specs):
printer init feat-new-endpoint  # writes specs/NNN-feat-new-endpoint.md
```

This creates the `.printer/` task store and `.codegraph/` index directory.

### 6. Run

```sh
printer exec spec.md             # run + review in one command
printer exec spec.md --verbose   # with live progress output
printer exec spec.md --agent opencode            # use opencode one-shot backend
printer exec spec.md --agent acp:opencode-acp    # use opencode ACP (persistent sessions)
printer test spec.md             # click-test a UI/web change with the computer tool
```

`printer test` drives one agent turn that exercises the running app end-to-end
through the `computer` CLI (input synthesis + screenshots). It runs on the host
because a real display is required, gates on a usable display **and** `computer`
being on PATH, and exits non-zero unless the verdict is PASS — so it works as a
CI/gate check. Pass `--url http://localhost:PORT` to point it at the app.


## Orchestration
The simplest way to kick off is to use the `exec` command, this is equivalent to running `printer run spec.md && printer review spec.md`.
