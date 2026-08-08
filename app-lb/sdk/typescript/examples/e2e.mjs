// The same script the Rust SDK's e2e runs: create → wait → exec → token → job →
// delete, against a live app-lb.
import { Serverctl, NotFoundError, NoRunningVmError, ForbiddenError, UnauthorizedError } from "../dist/index.js";

const SERVER = process.env.APP_LB ?? "127.0.0.1:34294";
let ok = 0, fail = 0;
const check = (label, got, want) => {
  const pass = JSON.stringify(got) === JSON.stringify(want);
  if (pass) { ok++; console.log(`  ok    ${label}`); }
  else { fail++; console.log(`  FAIL  ${label}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`); }
};
const truthy = (label, v) => check(label, !!v, true);

const admin = new Serverctl({ server: SERVER, user: "admin", password: "hunter2" });

console.log("== reachability and gates ==");
await admin.healthz();
ok++; console.log("  ok    healthz");
const gates = await admin.gates();
check("both tiers are gated", gates, { view: true, crud: true });

console.log("\n== an unauthenticated client is refused, and says why ==");
const anon = new Serverctl({ server: SERVER });
try { await anon.deployments(); fail++; console.log("  FAIL  anon should be refused"); }
catch (e) {
  truthy("UnauthorizedError", e instanceof UnauthorizedError);
  truthy("names the missing credential", e.message.includes("no credential was sent"));
  truthy("isAuth", e.isAuth);
  truthy("not retryable", !e.retryable);
}

console.log("\n== create and wait ==");
await admin.createDeployment({
  id: "ts-sb", routes: [],
  vm: { driver: "firecracker", port: 38080 },
  scaling: { min_replicas: 0, max_replicas: 1 },
});
ok++; console.log("  ok    created");
const ready = await admin.waitForReady("ts-sb", { pollMs: 100, timeoutMs: 10_000 });
check("scaled to zero has converged", ready.kind, "vm");

console.log("\n== a missing deployment is typed and named ==");
try { await admin.deployment("nope"); fail++; }
catch (e) {
  truthy("NotFoundError", e instanceof NotFoundError);
  check("names the kind", e.kind, "deployment");
  check("names the object", e.name_, "nope");
}

console.log("\n== exec with no VM and no wake ==");
try { await admin.exec("ts-sb", "echo hi", { wake: false }); fail++; }
catch (e) {
  truthy("NoRunningVmError", e instanceof NoRunningVmError);
  check("names the deployment", e.deployment, "ts-sb");
}

console.log("\n== app-tokens ==");
const minted = await admin.mintToken({ name: "ts-agent", admin: "admin", deployments: ["ts-sb"] });
truthy("the secret is returned once", minted.token.startsWith("applb_"));
check("scoped as asked", minted.deployments, ["ts-sb"]);

const agent = new Serverctl({ server: SERVER, token: minted.token });
check("the token reaches its own deployment", (await agent.deployment("ts-sb")).spec.id, "ts-sb");

try { await agent.deployments(); fail++; console.log("  FAIL  a scoped token should not list the fleet"); }
catch (e) {
  truthy("ForbiddenError on a fleet-wide route", e instanceof ForbiddenError);
  truthy("403 explains the scope", e.message.includes("scoped"));
}

console.log("\n== re-scope keeps the secret working ==");
await admin.patchToken(minted.id, { deployments: ["*"] });
check("wider now", (await agent.deployments()).length >= 1, true);

console.log("\n== metrics narrow themselves for a scoped token ==");
await admin.patchToken(minted.id, { deployments: ["ts-sb"], admin: "view" });
const m = await agent.metrics();
check("sees only its own", m.deployments.map((d) => d.id), ["ts-sb"]);
check("and the fleet rollup matches", m.fleet.deployments, 1);

console.log("\n== revocation is immediate ==");
await admin.revokeToken(minted.id);
try { await agent.metrics(); fail++; }
catch (e) { truthy("dead after revoke", e instanceof UnauthorizedError); }

console.log("\n== a job, polled to completion ==");
await admin.createDeployment({
  id: "ts-site", routes: [{ host: "ts.example.com" }],
  site: { root: "/tmp" },
  update: { working_dir: "/tmp", commands: ["true"], verify_timeout_secs: 0 },
});
const job = await admin.startUpdate("ts-site");
const lines = [];
const done = await admin.waitForJob(job.id, { pollMs: 200, timeoutMs: 30_000, onProgress: (p) => lines.push(...p.newLog) });
truthy("the job finished", done.status !== "running");
check("no line was reported twice", lines.length, new Set(lines).size);

console.log("\n== delete ==");
await admin.deleteDeployment("ts-sb");
await admin.deleteDeployment("ts-site");
check("gone", await admin.deploymentExists("ts-sb"), false);

console.log(`\n${ok} ok, ${fail} failed`);
process.exit(fail ? 1 : 0);
