# Security Policy

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 0.1.x   | ✅ Yes     |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Email **security@droco.io** with:

- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested fixes (optional)

You'll receive an acknowledgment within **48 hours** and a status update within **7 days**.

If the vulnerability is confirmed, we'll:
1. Work with you on a fix
2. Credit you in the release notes (unless you prefer to remain anonymous)
3. Publish a patch release as soon as possible

## Scope

SessionGuard runs as a user-space daemon with access to your project directories and a local SQLite database. It does not make network requests during normal operation. Please report any issues related to:

- Arbitrary file read/write outside watched directories
- Privilege escalation
- Path traversal in reconciliation logic
- Malicious tool definition TOML injection

## Out of Scope

- Issues requiring physical access to the machine
- Social engineering
