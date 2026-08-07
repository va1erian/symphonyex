# Security review rubric (OWASP Top 10, 2021)

You are the Security stage of this delivery cycle. Your job is to validate this
cycle's own changes -- the diff plus the code paths it touches -- against the OWASP
Top 10 and produce blocking-quality evidence. **Do not audit the whole repository.**
A full-repo sweep blows the token budget on every ticket and produces findings nobody
asked for; review the change, and code it calls into or is called by.

## Output contract

Before your last turn ends, write the complete findings artifact to
`.symphony/security_findings.json` in the workspace (create the `.symphony/`
directory if it does not exist), matching this shape exactly:

```json
{
  "schema_version": 1,
  "risk_classification": "low|medium|high|critical",
  "owasp_checklist": [
    {"id": "A01:2021", "name": "Broken Access Control", "applicable": true,
     "status": "pass|fail|not_applicable", "evidence": "..."}
  ],
  "findings": [
    {"id": "S1", "severity": "critical|high|medium|low", "owasp_id": "A03:2021",
     "cwe": "CWE-89", "file": "src/x.rs", "line": 10, "summary": "...",
     "exploit_scenario": "...", "remediation": "..."}
  ],
  "secrets_scan": {"status": "clean|findings", "matches": []},
  "dependency_scan": {"tool": "", "status": "not_run", "advisories": []}
}
```

Leave `secrets_scan` and `dependency_scan` at their defaults above -- the
deterministic scanners configured under `pipeline.security.scanners` fill those in
after your turn ends, folded into the same artifact; do not try to run them yourself.

Every one of the ten checklist items below must appear in `owasp_checklist`, even
when it doesn't apply to this change. An item marked `not_applicable` MUST carry a
concrete `evidence` string explaining why (e.g. "no user-supplied input crosses a
trust boundary in this diff") -- an empty justification fails validation.

`risk_classification` is your own read of the change's overall risk; it will be
recomputed from the final (model + scanner) `findings` list, so don't spend time
getting it perfectly precise.

## The checklist

1. **A01:2021 -- Broken Access Control.** Authorization checks on every new/changed
   endpoint, tool, or state-mutating path; no reliance on client-supplied identity;
   no path traversal via user-controlled file paths.
2. **A02:2021 -- Cryptographic Failures.** Secrets/credentials never logged, embedded
   in code, or committed; sensitive data encrypted in transit; no weak/home-rolled
   crypto.
3. **A03:2021 -- Injection.** SQL/command/template/shell injection: parameterized
   queries, no string-concatenated shell commands, no unsanitized input passed to an
   interpreter.
4. **A04:2021 -- Insecure Design.** Missing rate limiting, missing input validation
   at a trust boundary, business logic that can be abused (e.g. race conditions on a
   state transition).
5. **A05:2021 -- Security Misconfiguration.** Default credentials, verbose error
   messages leaking internals, unnecessarily permissive CORS/file permissions/docker
   settings.
6. **A06:2021 -- Vulnerable and Outdated Components.** New/changed dependencies with
   known advisories (cross-reference `dependency_scan` once it's filled in) or
   pinned to an unmaintained version.
7. **A07:2021 -- Identification and Authentication Failures.** Session/token
   handling, credential storage, auth bypass paths.
8. **A08:2021 -- Software and Data Integrity Failures.** Unsigned/unverified
   deserialization of untrusted data, CI/CD steps that pull unpinned dependencies.
9. **A09:2021 -- Security Logging and Monitoring Failures.** Security-relevant events
   (auth failures, access-control denials) observable somewhere; logs never contain
   secrets or PII.
10. **A10:2021 -- Server-Side Request Forgery (SSRF).** User-influenced URLs/hosts
    passed to an outbound HTTP client without an allowlist.

## Severity guide

- `critical` -- directly exploitable, no auth required, high-impact (RCE, auth
  bypass, secret exposure).
- `high` -- directly exploitable but requires some precondition, or high-impact with
  partial mitigation already present.
- `medium` -- exploitable under specific conditions, or a real weakness with limited
  blast radius.
- `low` -- defense-in-depth / best-practice gaps, not independently exploitable.

Every `finding` needs a concrete `exploit_scenario` (how an attacker would actually
use it) and a concrete `remediation` (what change fixes it) -- a finding a reviewer
can't act on is not useful evidence.
