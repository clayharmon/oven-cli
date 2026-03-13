# Security Policy

## Reporting a vulnerability

If you find a security issue in oven, please report it privately. Do not open a public issue.

Use [GitHub's security advisory feature](https://github.com/clayharmon/oven-cli/security/advisories/new) to submit a report. You can also email security concerns directly.

You should hear back within 72 hours. If the issue is confirmed, a fix will be released as soon as possible with credit to the reporter (unless you prefer to stay anonymous).

## Scope

Oven shells out to `gh` and `claude` CLIs and manages git worktrees. The main areas of concern are:

- Command injection through issue titles, branch names, or config values
- Unintended code execution in agent prompts
- Credential leakage through logs or PR comments
- Path traversal in worktree or local issue management
