# Changelog

All notable changes to the LingoHub CLI. Versions follow [semantic versioning](https://semver.org).

## 0.2.1 - 2026-11-05

- `lingohub tui` keeps working when the API stops sending a workspace's last activity and a
  project's open-source flag; the dashboard no longer shows either value

## 0.2.0 - 2026-11-05

- `lingohub login` signs in through the browser: after you confirm the login in the LingoHub app,
  it stores a new personal API key in `~/.lingohub`, so keys no longer need to be copied by hand
- `lingohub logout` revokes the stored API key and removes it from `~/.lingohub`
- `lingohub api call deleteApiKey` revokes the personal API key that makes the request

## 0.1.0 - 2026-11-04

First public release.

- `lingohub` command for the LingoHub API V2 (public beta): workspace, stack, project and
  style guide commands, generic operation calls, uploads, downloads and job polling
- `lingohub tui` terminal dashboard and `lingohub api mcp-serve` MCP server
- Installers for macOS, Linux and Windows, a Homebrew formula and static Linux binaries
- `lingohub upgrade` and a daily update notice in interactive terminals
