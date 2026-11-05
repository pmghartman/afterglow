# Security

Report a vulnerability privately through GitHub's [security advisories](https://github.com/pmghartman/afterglow/security/advisories/new), not a public issue.

## What to know

- The host's link listens on `127.0.0.1` only. Its token lets whoever holds it launch apps and type into the console's session: an installer must keep the config and link file readable by SYSTEM and Administrators only.
- The host service runs as SYSTEM.
- glow talks to a local Sunshine or Apollo over loopback; the certificate it pairs with is a credential for that host.
- Release zips carry a build provenance attestation: `gh attestation verify <zip> --repo pmghartman/afterglow`.
