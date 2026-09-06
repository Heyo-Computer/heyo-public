# Deploying heyo-mcp as a VM

The alternative to `deploy/heyo-mcp.json` + `deploy/supervisor/heyo-mcp.conf`,
which run this as a host process behind app-lb. Both shapes are supported; this
one exists because of what the host-process shape *lets the server do*.

## Why

Inbound traffic was never the problem. A `proxy_pass` deployment is fully under
app-lb: TLS, the app-token gate, and a health probe of the upstream every
autoscaler tick, which drops it from `select` and raises a feed event when it
stops answering (`app-lb/src/autoscale.rs:626-670`).

The **outbound** half was. As a host process this server reaches
`127.0.0.1:9090`, `:9600` and `:9555` directly — app-lb's admin listener,
app-obs and ci, each *behind* a gate it never passes through. Being a host
process is what grants that. A VM cannot do it, so every call goes in the front
door as a scoped principal app-lb verifies.

Nothing is exposed to make this work. app-lb's :443 is already public, guests
already have egress (`mvm-ctrl/src/driver/tap_networking.rs:470-500`), and
`heyo-cloud.json` already reaches a host service by public hostname. The admin
listener stays on 127.0.0.1:9090, untouched.

## The credential posture, which is the point

**`APPLB_TOKEN` is deliberately absent from the spec.** With no app-lb
credential of its own, `withForwardedAuth` (`src/config.ts`) hands every app-lb
call the caller's own `applb_…` token, and app-lb scope-checks it *twice*:

1. the gate on the admin deployment (`auth.rs:315-330`, `token.admits(...)`), and
2. the admin listener itself, which accepts `Authorization: Bearer applb_…`
   against the same `TokenStore` and applies `satisfies_in`
   (`app-lb/src/admin.rs:616-646`).

One credential, two enforcement points, no conflict — the proxy forwards
`Authorization` unchanged, stripping only `IDENTITY_HEADERS`
(`app-lb/src/proxy.rs:700-702`).

This is also what makes the VM's `0.0.0.0` bind acceptable. See below.

## Preconditions (not in this manifest — apply separately)

1. **Add `app-token` to the admin deployment's gate.** Confirm the id first
   (`applb_list_deployments`; it fronts `127.0.0.1:9090` on `admin.us2`), then
   add `"provider": ["google", "app-token"]` to its `auth`. Do **not** add
   `public_paths` there — `app-lb/src/main.rs:496-540` explains why a
   prefix-matched public path on that deployment is unauthenticated RCE.

2. **Add `app-token` to `obs.json`'s gate**, the same way. `ci.json` already
   has it.

3. **Unset `APP_OBS_API_TOKEN` on the app-obs process.** There is one
   `Authorization` header and, through a gate, two things that want it. Resolve
   it the way ci already does: let the gate be the authenticator. app-obs's
   listener stays on `127.0.0.1:9600`, so this is not an exposure.

4. **Populate the `mcp` secret** with two keys, both minted `applb_` tokens
   that exist only to clear a gate — `admin: none`, one deployment each:
   - `obs_gate_token` — `deployments: ["app-obs"]`
   - `ci_gate_token` — `deployments: ["ci"]`

   There is deliberately no cloud key here. See below.

## Minting caller tokens

A caller's reach is their token's scope, and going through the front door adds
one requirement: **the token must also admit the admin deployment**, because
`admits()` is checked against the deployment the *gate* belongs to before the
admin listener ever sees the request. So either

- list it alongside the targets: `deployments: ["app-lb-admin", "fastcar"]`, or
- confine by namespace with an empty `deployments` list, which admits everything
  in that namespace (`app-lb/src/tokens.rs:155-163`).

The `admin` axis is unchanged and still coarser than it sounds: `view` reaches
only `/metrics`, `/disks`, `/feeds`, `/security`, `/ingress`, `/storage`. There
is no read-only tier for deployment routes — `GET /deployments/:id` is CRUD-tier
because a spec's env vars can hold secrets — so a token that can *read* a
deployment can also delete it. `env_from` above is why this spec has nothing
worth reading.

## This deployment has no sandbox tools, on purpose

An app-token gate admits `applb_…` and nothing else, and cloud has never heard
of that credential. So there is no cloud key here and no way for a caller to
supply one — which makes the fourteen `sandbox_*` tools plus `heyo_capacity`
unreachable by construction, not by misconfiguration.

`buildTools` (`src/server.ts`) therefore omits them when no cloud credential is
present, and `withForwardedAuth` refuses to substitute an app-lb token for one:
borrowing it would have produced a tool list that is complete, advertised, and
401s on every call. `heyo_status` still probes cloud and reports it as not
configured, so the absence stays an answerable question.

The gate is on the *credential*, not on a deployment-wide switch, so the same
build still serves the multi-tenant shape: an instance carrying no key of its
own gives the sandbox tools to a caller who presents a `heyo_api_*` key, per
request. That is a different deployment — no app-lb gate, since the gate would
reject a cloud key at the door — and it is the natural home for customer
sandbox access if it is ever wanted.

What is left here is the fleet: app-lb, app-obs, ci, and the feed.

## The 0.0.0.0 bind, and the rule that replaces the loopback one

app-lb reaches the guest over the tap, so the server must bind `0.0.0.0`. That
matters more than it looks: heyvmd installs a blanket
`-s 172.16.0.0/12 -d 172.16.0.0/12 -j ACCEPT` in the host's FORWARD chain
(`mvm-ctrl/src/driver/tap_networking.rs:509-534`), so **every other VM on the
host, customer sandboxes included, can route to this guest**. A naive port of
this deployment would relocate the exact hole the loopback bind existed to
close.

Two things close it, in this order:

1. **Nothing worth stealing.** No `APPLB_TOKEN`, so a peer VM reaching :9650
   directly gets a server that acts as whoever called — and it has no app-lb
   credential to act with. The `obs`/`ci` tokens it *does* hold are `admin: none`
   and scoped to one deployment each.
2. **`init.sh` firewalls INPUT** to the host end of the /30 (tcp/9650 and
   tcp/22). Best-effort and logged if unavailable, because a boot that cannot
   firewall itself is not worth wedging — it is the second layer, not the first.

`HEYO_MCP_REQUIRE_IDENTITY=0` is still required, and still only by the
app-token gate, which forwards no identity by design ("a token is not a
person"). Under this shape the old justification — "and the listener is
loopback" — no longer holds, and the two items above are what stands in for it.

## Building

`heyvm mvm build` runs docker build → docker create → docker export → mke2fs,
so only the filesystem survives; the kernel boots `init=/init.sh` and app-lb
starts node afterwards via `start_command`, the only channel carrying the env
vars above.

    # locally, from mcp/
    heyvm mvm build --local-only -f deploy/image/Dockerfile -c . -n heyo-mcp

    # on the fleet
    applb_build_deployment heyo-mcp     # POST /deployments/heyo-mcp/build

The `build` block pairs `"dockerfile": "mcp/deploy/image/Dockerfile"` with
`"context": "mcp"`, keeping the context to this subtree. There is no `update`
block: a VM deployment ships a new image, it does not `git pull` in place.

## Known gap

`withForwardedAuth` forwards a caller's `applb_` token to `applb` only — `obs`
and `ci` are not in it (`src/config.ts`). That is why this spec still holds two
static gate tokens, and they are now the *only* credentials in the VM. Extending
the same rule to obs and ci would let those calls run as the caller too and take
the VM to zero. Worth doing; not done here.
