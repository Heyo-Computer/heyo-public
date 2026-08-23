/**
 * Who the caller is, when this runs behind app-lb's gate.
 *
 * **The gate verifies the JWT; this does not re-verify it.** app-lb checked the
 * signature, the issuer, the audience and the `require` claims before forwarding
 * anything, and it strips `x-auth-request-*` unconditionally before setting
 * them, so those headers cannot be spoofed by a client. Re-verifying here would
 * need the HMAC secret — the same value that *mints* tokens — in a second
 * process, which is a larger risk than the one it removes.
 *
 * What is checked is that the headers are present at all. The listener binds
 * loopback and app-lb is the only thing expected to reach it, so a request
 * arriving without an identity did not come through the gate. That is either a
 * misconfiguration — the deployment's `auth` block missing, or the route not
 * gated — or something on the box talking to the port directly. Both should
 * fail loudly, because every tool behind this can delete a deployment.
 *
 * The token itself stays in the `Authorization` header where the caller put it.
 * app-lb deliberately does not copy claims into headers: the app can read the
 * signed token if it wants more than the identity below.
 */

export interface Identity {
  /** Whichever header identified the caller, for logging. */
  who: string;
  user?: string;
  email?: string;
  name?: string;
}

export class Unauthenticated extends Error {
  constructor(message: string) {
    super(message);
    this.name = "Unauthenticated";
  }
}

/**
 * `user` first, and that ordering is the whole correctness of this function.
 *
 * Under a JWT gate `x-auth-request-user` carries `subject_claim`, which app-lb
 * calls the one claim a gate cannot do without and refuses a token for missing.
 * `x-auth-request-email` carries `email_claim` and is set with
 * `unwrap_or_default()`, so a perfectly valid token from an issuer that sends no
 * email arrives with that header **empty**. Keying on email would reject those
 * callers while telling them they had bypassed the gate — the opposite of what
 * happened. A Google gate populates both, so preferring `user` costs nothing
 * there.
 */
export function identityFrom(headers: Record<string, string | string[] | undefined>): Identity {
  const one = (k: string): string | undefined => {
    const v = headers[k];
    const first = Array.isArray(v) ? v[0] : v;
    return first && first.trim() ? first : undefined;
  };
  const user = one("x-auth-request-user");
  const email = one("x-auth-request-email");
  const who = user ?? email;
  if (!who) {
    throw new Unauthenticated(
      "No x-auth-request-user or -email on this request, so it did not arrive through " +
        "app-lb's gate. This process binds loopback and expects app-lb in front of it: " +
        "check that the deployment's auth block is present and that this path is not in " +
        "public_paths. Set HEYO_MCP_REQUIRE_IDENTITY=0 only for local testing.",
    );
  }
  return { who, user, email, name: one("x-auth-request-name") };
}

export function identityRequired(env: NodeJS.ProcessEnv = process.env): boolean {
  return env.HEYO_MCP_REQUIRE_IDENTITY !== "0";
}
