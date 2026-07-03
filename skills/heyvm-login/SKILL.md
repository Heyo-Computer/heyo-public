---
name: heyvm-login
description: Log in to the Heyo platform via `heyvm login` — authenticate with email/password or API key to enable cloud sandbox operations (deploy, list, archive, etc.).
argument-hint: "[--api-key <KEY> | --email <EMAIL>]"
allowed-tools: Bash, Read, Grep
---

# heyvm login — Heyo Platform Authentication

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills.

You are helping the user log in to the Heyo platform so they can use cloud features (deploying sandboxes, archiving, listing deployed sandboxes, etc.).

## Binary Location

The binary is `heyvm`. If it is not on PATH, build it from `mvm-ctrl/`:

```bash
cargo build --release -p heyvm
```

## Login Methods

### 1. Interactive email/password (default)

```bash
heyvm login
```

Prompts for email, then securely prompts for password (hidden input). This is the simplest flow for most users.

You can also provide email up front:

```bash
heyvm login --email user@example.com
```

### 2. API key

```bash
heyvm login --api-key <KEY>
```

Exchanges the API key for a JWT token. Also persists the key to `~/.heyo/.env` so the CLI can refresh tokens automatically.

### 3. Non-interactive (CI / scripting)

```bash
heyvm login --email user@example.com --password 'secret'
```

Both email and password can be passed as flags for automated workflows. Avoid this in shared shell history.

## Options

| Flag | Description |
|------|-------------|
| `--api-key <KEY>` | Authenticate with an API key |
| `--email <EMAIL>` | Email address for login |
| `--password <PASS>` | Password (omit to be prompted securely) |

## What Happens on Login

1. Credentials are sent to the Heyo auth server (`https://auth.heyo.computer` by default, or the URL set via `--auth-url` / `AUTH_SERVER_URL`).
2. On success, a JWT token is saved to `~/.heyo/token.json`.
3. For API key login, the key is also saved to `~/.heyo/.env` for automatic token refresh.
4. Subsequent CLI commands (`list`, `archive`, `deploy`, `connect`, etc.) use the stored token automatically.

## Checking Login Status

Running `heyvm login` with no arguments when already authenticated will display the token expiry and exit without re-authenticating.

## Token Storage

| File | Contents |
|------|----------|
| `~/.heyo/token.json` | JWT access token, refresh token, expiry |
| `~/.heyo/.env` | API key and auth server URL (API key login only) |

## Dev Mode

Use `--auth-url` or the `--dev` flag on the parent `heyvm` command to target a local auth server:

```bash
heyvm --dev login --email dev@localhost.com
```

This sends credentials to `http://localhost:3001` instead of production.

## Workflow

When the user asks to log in or authenticate:

1. Ask whether they have an API key or want to use email/password.
2. Run the appropriate `heyvm login` command.
3. Verify success by checking the output message.

When the user provides `$ARGUMENTS`, pass them directly to `heyvm login`. For example, `/heyvm-login --api-key abc123` should run `heyvm login --api-key abc123`.

If `$ARGUMENTS` is empty, run `heyvm login` interactively (the CLI will prompt for credentials).
