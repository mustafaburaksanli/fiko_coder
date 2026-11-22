# Security policy

## Supported status

FİKO CODER is experimental and no release is currently supported for production or security-sensitive use. Security fixes may be developed on feature branches before release.

## Reporting vulnerabilities

Please do **not** disclose exploit details, credentials, or private project content in public GitHub issues. Use GitHub's **Report a vulnerability** / private vulnerability reporting feature if enabled, or contact the repository owner privately using their published GitHub contact information. Do not assume private reporting is enabled.

## Threat model and known limitations

- Model output, source files, fetched content, and PostgreSQL memory are untrusted data.
- Approval must be obtained through the trusted terminal prompt, not a model-generated message.
- A verified candidate is not permission to write arbitrary files: target/path checks, current project identity and user approval must be checked again.
- Cargo verification runs real code via build scripts, macros, and tests. Temporary source folders **do not** isolate filesystem, network, processes, or credentials.
- Multi-file patch and rollback are not fully crash-atomic. External writers can race filesystem checks. An interrupted workspace operation may leave a pending marker in `.fiko_backups`; read-only `--recovery-audit` helps identify target and backup integrity, but does not restore automatically or guarantee durability.
- Review changes and maintain independent backups before any test.
- Never commit database URLs with real credentials, tokens, private keys, or personal data.
- Do not run tests against production databases. The opt-in PostgreSQL tests require explicitly configured local test databases.

## Responsible disclosure

Include affected commit/version, expected versus observed behavior, sanitized reproduction steps, and the potential impact. Please omit real credentials and user data.
