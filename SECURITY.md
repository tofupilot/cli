# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately. **Do not open a public issue.**

Email **security@tofupilot.com** with:

- a description of the issue and its impact,
- steps to reproduce or a proof of concept,
- the CLI version (`tofupilot --version`) and your OS/platform.

We aim to acknowledge reports within 3 business days and will keep you informed
of remediation progress. Please give us a reasonable window to ship a fix before
any public disclosure.

## Scope

This policy covers the TofuPilot CLI in this repository. Issues in the hosted
TofuPilot service should also be sent to security@tofupilot.com.

## Handling of credentials

The CLI stores credentials in `~/.tofupilot/credentials.json` and local state in
`~/.tofupilot/state.redb`. Never paste the contents of these files into issues,
logs, or pull requests.
