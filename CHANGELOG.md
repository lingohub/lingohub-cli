# Changelog

All notable changes to the LingoHub CLI. Versions follow [semantic versioning](https://semver.org).

## 0.3.0 - 2026-11-17

- The bundled API V2 contract matches the current API (OpenAPI 3.1, 247 operations). API V2 is
  in beta and this sync removes and renames operations:
  - Texts are part of their segment: `getSegment` and `listSegments` return them, `updateSegment`
    changes them through a `texts` map and `transitionSegment` changes a text's status. The
    `getText`, `listTexts`, `updateText` and `transitionText` operations are gone
  - Batches are atomic and return the created keys or changed segments as a list:
    `createKeysBulk`, `updateSegmentsBulk` and `transitionSegmentsBulk` (previously
    `updateTextsBulk` and `transitionTextsBulk`). The per-item `BULK_RESULT` and its
    `bulk_incomplete` error no longer exist; a rejected batch is an ordinary API error
  - Project settings are one document: `getProjectSettings` and `updateProjectSettings` replace
    the format, key, placeholder, import and export settings operations
  - An integration is changed with `updateProject<Provider>Integration`, which replaces the Git
    connection, sync, push and selection settings operations and the content selection settings;
    `sync<Provider>Integration` replaces `create<Provider>Sync`
  - `updateKey` renames a key (`renameKey` is gone); `listImportChanges` replaces the import
    resource and text change lists; LINA quality checks use `getLinaQualityCheckSettings` and
    `updateLinaQualityCheckSettings`; an audit log entry and a webhook delivery carry their
    changes and request inline
  - `translateLina` takes plain texts and returns the results in the order of the input
  - No longer part of API V2: creating, revoking and rotating workspace API keys and their
    permissions, CDN keys, deleting distributions and releases, activating a release, release
    and LINA run logs, cancelling imports and LINA runs, import inspection, CSV rows and
    workspace-wide capabilities (`getProjectCapabilities` remains)
- `lingohub api download-distribution-release` reads the download link from the release itself
  and refuses a release that has not succeeded
- `lingohub api wait getLinaExecution` takes `--path linaExecutionId=...` (was `executionId`)
- `deleteProjectLanguage` keeps the stack language unless
  `--query delete_unused_stack_language=true` is passed; it used to delete an unused one by default
- The interactive `lingohub stack create` asks for the name and source language; stacks no longer
  take a type on creation
- `lingohub api mcp-tools` and `api mcp-serve` describe nullable inputs as the API does and
  expose 245 JSON operations

## 0.2.3 - 2026-11-10

- The bundled API V2 contract matches the current API: segments and new keys no longer have an
  `ai_context` field (LINA reads a segment's `description` instead), and GPT 6 Luna, GPT 6.1
  Terra, GPT 6.1 Sol, Gemini 3.8 Flash, Claude Haiku 5.5, Claude Sonnet 5.5 and Claude Opus 5.5
  are known linguistic engine models

## 0.2.2 - 2026-11-07

- The bundled API V2 contract matches the current API: the Tasks operations are gone, and
  Mistral Large 4 is a known linguistic engine model

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
