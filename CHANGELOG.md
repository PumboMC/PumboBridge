# Changelog

All notable changes to PumboBridge, one section per release. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.3-beta] - 2026-10-30

Update PumboBridge together with PumboPerms.

### Added

- Pumbo plugin permissions from the proxy, so `pumbo.bans.*` and `pumbo.*` work on the servers.
- PumboPerms on a server checks permissions from what the proxy sent and knows when the proxy's PumboPerms rules.

### Changed

- Builds for Pumpkin 0.2.1; Pumpkin 0.2.0 has its own file.
- Only PumboPerms can make the bridge stop or resume writing permissions.
- A local PumboPerms imports only ranks from the current proxy connection, not an old copy from disk.
- While ticks are frozen (`/tick freeze`), the bridge still answers the proxy when something happens on the server.
- An unchanged `config.yml` follows updates; for a changed one the log names the new options.
- Clearer log lines when the proxy connection drops and comes back.
- Crashes are logged with their place in the code.

### Fixed

- `/tp`, `/xp`, `/banip` and `/pardonip` after `run` in `/execute` without a permission check on Pumpkin 0.2.0 and 0.1.0-dev.
- Proxy commands running after frozen ticks, long after the proxy gave up on them.
- A failed save of `perms-export.json` leaving a broken file without a log line.

## [0.1.2-beta] - 2026-10-09

No changes in PumboBridge; released with PumboPerms 0.1.2-beta.

## [0.1.1-beta] - 2026-10-09

No changes in PumboBridge; built with the shared library of 0.1.1-beta.

## [0.1.0-beta.1] - 2026-10-09

First public beta.

[0.1.3-beta]: https://github.com/PumboMC/PumboBridge/compare/v0.1.2-beta...v0.1.3-beta
[0.1.2-beta]: https://github.com/PumboMC/PumboBridge/compare/v0.1.1-beta...v0.1.2-beta
[0.1.1-beta]: https://github.com/PumboMC/PumboBridge/compare/v0.1.0-beta.1...v0.1.1-beta
[0.1.0-beta.1]: https://github.com/PumboMC/PumboBridge/releases/tag/v0.1.0-beta.1
