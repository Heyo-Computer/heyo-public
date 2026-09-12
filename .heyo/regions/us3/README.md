# Parallel us3 installation

These resources add a new installation. Do not deregister a staging backend,
replace staging services, move staging runners, or copy staging deployment history.
The existing Auth service and S3 object store can be shared without moving them.

## Cloud

`cloud.json` is an inert app-lb template, with no routes and zero replicas.
Cloud and Orchestrator use the new `orchestrator_us3` database together: the
current Orchestrator reads Cloud's deployment-status table directly. Never point
this instance at the existing `cloud/database-url` or `orchestrator/database-url`.

Use the existing verified us3 Ubuntu Firecracker runtime image (the image of
`orchestrator-us3`) for `vm.image`. Its Orchestrator binary is not started in this
Cloud VM. The Cloud executable and migrations come from an independently
SHA-256-verified release archive, not from that runtime image.

Render `start-cloud.sh` into the deployment's start command: write its base64
contents to `/tmp/start-cloud.sh`, then daemonize `bash /tmp/start-cloud.sh` with
stdin from `/dev/null` and output redirected to `/tmp/cloud.log`. app-lb injects
the referenced secrets; never embed their values in the launcher or this repo.
The script signs its S3 download at boot, so it does not depend on a staging
service or an expiring presigned URL. Verify the archive's recorded deployment
digest before recording its permanent URL/digest in HeyoSecret under
`cloud-us3/source-archive-url` and `cloud-us3/source-archive-sha256`.

The `cloud-us3` app-lb delivery secret holds those release references and the
existing `cloud/s3-*` values. Existing keys are not rotated. Archive cleanup in
the verified Cloud release selects records from its own database; it does not
enumerate and purge unrelated objects in the shared bucket.

Start one replica, establish the existing narrow Postgres IP/tap grant for that
candidate, and verify migrations and health before publishing
`cloud.us3.heyo.work`. Cloud's own bearer authentication protects its APIs;
verify anonymous rejection and authenticated internal requests through HTTPS.
The template leaves NATS off until a regional broker is configured. API health
alone is not complete CI/CD verification.

Backend registration must be additional and confined to the new Cloud database.
Do not change the existing daemon's staging registration or callback destination.
Inspect registration/reconciliation effects before connecting it to a backend
that also hosts existing platform VMs.
