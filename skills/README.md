# Heyo Skills

Reusable agent skills for the Heyo platform, `heyvm`, `printer`, `computer`,
and `codegraph`.

This directory folds in the public `Heyo-Computer/skills` repository and is the
skills source of truth in the public Heyo monorepo. The private Heyo monorepo
consumes these files through its `heyo-public/` submodule.

## Skills

| Skill | Use it when you want to… |
|-------|--------------------------|
| **heyvm-docs** | Get oriented — a high-level overview of the platform and an index that points to the right skill for a task. Start here. |
| **heyvm-login** | Authenticate to the Heyo platform (`heyvm login`). The first step before any cloud operation. |
| **heyvm-system** | Diagnose host setup (KVM/Firecracker on Linux, Apple Virtualization on macOS) and run end-to-end smoke tests. |
| **heyvm-sandbox** | Manage local sandbox lifecycle: `create / start / stop / restart / list / exec`, mounts, ports, backend selection. |
| **heyvm-deploy** | Push code to a Heyo cloud sandbox, bind ports, set up custom domains, and manage deployed sandboxes. |
| **heyvm-firecracker** | Author Dockerfiles that produce Firecracker rootfs images and build them with `heyvm mvm`. |
| **heyvm-proxy** | Expose ports and connect to remote sandboxes over iroh P2P (`proxy`, `connect`, `ssh`, `share`, `bind`). |
| **heyvm-database** | Create, list, and run SQL against Heyo cloud SQLite databases. |
| **codegraph-search** | Navigate and search code efficiently with the `codegraph` CLI. |
| **codegraph-edit** | Apply source edits with `codegraph patch`. |
| **computer** | Use the `computer` CLI for visual or interactive UI review. |
| **xdotool** | Drive X11 desktop automation where `xdotool` is available. |

Each individual skill lives in its own directory with a `SKILL.md` file and YAML
frontmatter (`name`, `description`, and optional metadata). Broader workflow
guides live alongside them, such as `SKILLS.md`.

## Using these skills

From the private Heyo monorepo checkout, edit skills under:

```text
heyo-public/skills/<skill-name>/SKILL.md
```

Commit and submit the change inside the `heyo-public` submodule first, then
commit and submit the updated private submodule pointer.

For tools that require a Claude-style skills directory, copy or symlink the
needed skill directories into the tool's expected location, for example:

```bash
mkdir -p ~/.claude/skills
ln -s "$PWD/skills/heyvm-sandbox" ~/.claude/skills/heyvm-sandbox
```

## Requirements

The `heyvm-*` skills drive the `heyvm` CLI, which must be on your `PATH`. Start
with **heyvm-system** to verify your host is ready, then **heyvm-login** to
authenticate.

The `codegraph-*` skills require the `codegraph` CLI. The `computer` skill
requires the `computer` CLI and a compatible desktop session.
