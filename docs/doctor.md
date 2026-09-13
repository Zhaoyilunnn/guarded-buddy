# Environment diagnostics

Run `buddy doctor` to inspect the current HOME configuration and local environment.
Checks print `PASS`, `WARN`, `FAIL`, or `SKIP` with next steps. Exit status is 1
when any check fails, otherwise 0. Invalid configuration is reported without
printing TOML contents, which may contain secrets.

There are no feature enable switches in the current configuration schema.
Doctor treats non-default weekly-report, signoff, or LLM options as configured
features. Explicit options equal to built-in runtime defaults still count as
configured; empty sections do not. Sync requires `sync.path`, email requires a
nonempty recipient list, and local history sources are detected by directory
presence. Unconfigured features are skipped rather than reported as missing.

Checks cover the selected AI executable or API key environment variable, endpoint
URL syntax, timeout, prompt template readability, local history directory access,
sync archive and device-home access, conventional mutt configuration paths,
signoff executables and workspaces, and output directory permissions. Missing
output directories are checked via their nearest existing ancestor, without
creating anything. Paths are resolved as in normal runs, relative to the current
working directory where applicable.

Doctor inspects the last 64 KiB of the newest `report.log` or `signoff.log` found within three
levels of the output root for historical error/timeout markers. These are warnings,
not proof of a current failure. A neighboring PID file is flagged for inspection;
process identity, duplicate workers, and stale PID status are not verified.
Log bodies, command arguments, API keys, and mail configuration contents are not
printed. Diagnostic paths can still reveal local filesystem information.

This command never invokes an AI backend or mutt, executes mail configuration,
sends email, makes network requests, or writes probe files. Authentication,
SMTP delivery, model availability, cloud replication, history format completeness,
and actual write success are not verified. A readable mutt configuration is not
proof that outgoing mail works. Source file discovery does not parse conversations.
Warnings explain checks that require manual verification.
