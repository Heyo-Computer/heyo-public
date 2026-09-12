# Native runners

Native runners execute jobs directly on independently owned Intel macOS or
Windows x86_64 hosts. They do not replace the existing heyvm runner path.

```yaml
jobs:
  mac:
    runs-on: [namespace-profile-mac-build, macos, macos-intel, x86_64-apple-darwin]
    steps:
      - run: cargo build --release
```

`runs-on` is mutually exclusive with `uses` and `vm`. Matrix expressions in
labels are expanded by the normal planner. The server matches **all** labels.

Set the same strong `CI_NATIVE_RUNNER_SECRET` on CI and the runner, then set
`CI_ENDPOINT`, `CI_NATIVE_RUNNER_ID`, and `CI_NATIVE_PROFILE=mac-intel` or
`windows-x64`. The agent refuses a profile unless the host OS and architecture
match; Apple Silicon must not register as the Intel heavy runner.

Registration, polling, source download, heartbeat and completion use dedicated
bearer-authenticated `/api/native/*` routes. Jobs and registrations are in
Postgres. A random expiring lease token fences late heartbeats, source reads,
artifact publication, and completions. Expired leases are recoverable, and
capacity is checked transactionally before leasing. Identical completion
retries are idempotent; changed evidence is rejected. Pending DAG advancement
is durable and retried by later runner polls.

Shell steps, general conditions, environment and output handoff, working
directories, timeouts, process-tree termination, logs, exit status,
`continue-on-error`, live cancellation, and `ci/upload-artifact` are supported.
Other repository actions and CI builtins fail loudly: put those in a dependent
Linux job. The server derives the final status from exact per-step evidence and
resolves and masks secrets again before writing logs; agent masking is only
defense in depth. If cancellation races an upload, sink bytes may be orphaned,
but the fenced artifact row and event are refused and never attached to the run.

## Isolated us3 acceptance

Build the agent on each native host with `cargo build --locked --release --manifest-path ci/Cargo.toml --bin native-runner-agent`.
Do not replace or stop its private-CICD agent for the trial. Use a separate
runner ID, working directory, and candidate CI endpoint. Agent processes run
trusted repository code directly on their hosts, not in a security sandbox.

The candidate CI service needs its own logical database, JetStream account/state,
public route and HeyoSecret-backed service configuration. Supply
`CI_NATIVE_RUNNER_SECRET` through that configuration; it is separate from
private `CICD_RUNNER_TOKEN` and the public CI submission token. Allow
`/api/native/` through the app-lb browser-auth gate; CI authenticates each request
itself. The checked-in us2 route example includes that exception but changing
the file does not change an installed route.

For an artifact-backed single-VM trial, `deploy/trial-service.json` is an
Orchestrator service template. Replace its owner/account, Linux runtime image,
network and admin-email placeholders. Package the Linux x86_64 `ci` and
`nats-server` binaries with `deploy/trial-start.sh` as `start.sh` and
`deploy/trial-nats.conf` as `nats.conf` at the archive root. Upload that archive
through Orchestrator's service archive API and attach its ID and exact source
revision to the deployment request. A local artifact build does not merge the
branch or submit CI; bypassing the normal CICD deployment path requires approval.

This template uses a dedicated Postgres database and the five
`ci-us3-trial/*` HeyoSecret references named in `env_from`. NATS runs only on
the VM's loopback interface; its monitoring port is not exposed. If either
process exits, the startup wrapper terminates the other and exits nonzero.
The template creates no public route: configure an authenticated app-lb route
only after checking that the VM port has no public bypass. A staging JWT gate
can forward the existing Auth identity. For browser email/password sign-in,
configure its `jwt.cookie` and `jwt.login_endpoint` (the existing staging
`/api/auth/login` endpoint) using an app-lb binary that supports those fields.
The gate verifies the returned token before setting the cookie. This is not
cross-domain SSO or a second Auth service inside the CI VM; Google sign-in
remains a separate gate configuration.

This is not an HA or automatic-upgrade deployment. NATS state, source trees,
logs and disk artifacts live in that VM's `/workspace/ci-state`; Postgres alone
does not preserve those files. The template deliberately retains previous VMs.
Preserve/export their state before replacement or retirement, and do not treat
this service VM as an ephemeral CI job cleanup target.

`native-smoke.yml` is an opt-in test workflow outside the active workflow
directory. Register it for the trial repository, or copy it into that
repository's `.ci/workflows/`. It tests native commands, conditions, outputs,
and archive upload without merging or deploying. Then test a failing command,
cancellation, reconnect/expired leases, and native validation dependencies in
the release workflow. All must pass on the actual Intel Mac and Dell before
declaring the us3 trial ready.

Current limitations: logs arrive with the completion report, not as a live
stream; output files support `name=value` lines (UTF-8 or PowerShell UTF-16),
not multiline delimiters. Source archives containing links are rejected.
Windows shells are PowerShell/pwsh; Bash is the macOS default. Release builtins
run in a dependent Linux job. Native runner registry UI is not included yet.
