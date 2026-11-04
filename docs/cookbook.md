# V2 workflow cookbook

Use public IDs from API responses in place of the placeholders below. These examples
are command recipes, not evidence of live provider testing. Read the operation's
schema and description in [the bundled OpenAPI contract](../contract/v2.json), or
inspect its input with `lingohub api mcp-tools --operation OPERATION`.

## Shared content and keys

A segment is shared content. Its keys are where it occurs in files, and its texts
hold the content of each language.

```sh
lingohub api call getSegment --path segmentId=seg_UUID --json
lingohub api call listSegmentKeys --path segmentId=seg_UUID --json
lingohub api call getText --path textId=txt_UUID --include-headers --json
lingohub api call updateText --path textId=txt_UUID \
  --file text-patch.json --if-match '"TEXT_ETAG"' --json
```

A text PATCH selects an active key through `key_id` and sends the complete ordered
`variants` array (`device`, `plural_form`, `content`). Do not resend response-only
`position` fields. Shared text edits affect every key of the segment.
`variants: []` clears content; null is invalid. Existing project automations still apply.

Key display names and the keys themselves have separate operations:

```sh
lingohub api call updateKey --path keyId=ress_UUID \
  --field 'name="Readable label"' --if-match '"KEY_ETAG"' --json
lingohub api call getKey --path keyId=ress_UUID --include-headers --json
lingohub api call renameKey --path keyId=ress_UUID \
  --file rename.json --if-match '"KEY_ETAG"' --json
```

Display names can change where structural keys are fixed. CSV keys cannot be
renamed or deleted; provider-controlled project types retain their restrictions.
A rename starts from `editable_key` and `group_key` in `getKey`. Rename, name
updates and deletion all use the key's validator; a rename returns the new one.
Use files or stdin for large escaped keys; normalization and key limits are
enforced by the backend.

## Create keys and handle partial outcomes

```sh
lingohub api call createKey --path resourceId=rc_UUID \
  --file new-key.json --idempotency-key create-sign-in --include-headers --json
lingohub api call createKeysBulk --path resourceId=rc_UUID --file keys.json --json
```

Each created key gets a fresh shared segment. A single key body contains `key`,
`is_plural` and `source_variants`, for example:

```json
{"key":"sign_in","is_plural":false,"source_variants":[{"device":"OTHER","plural_form":"OTHER","content":"Sign in"}]}
```

Bulk requests correlate items through their `reference` and returned `index`.
Inspect all outcomes, including `FAILED`, `UNKNOWN` and `NOT_PROCESSED`; exit 1
with a full response does not mean nothing committed. Text bulk operations
likewise preserve every per-item result. See the operation schema for exact bodies.

```sh
lingohub api call transitionText --path textId=txt_UUID \
  --field 'action="APPROVE"' --if-match '"TEXT_ETAG"' --json
```

## Import and export

```sh
lingohub api upload createImport --path branchId=prb_UUID \
  --upload ./de.json --form language_code=de --include-headers --json
lingohub api wait getImport --path importId=rin_UUID --json
lingohub api call listImportEntries --path importId=rin_UUID --json
lingohub api call getExport --path exportId=reg_UUID --include-headers --json
lingohub api wait getExport --path exportId=reg_UUID --json
lingohub api download-export reg_UUID --output export.zip --json
lingohub api download-export-entry reg_UUID ree_UUID --output de.json --json
```

Imports apply the existing immediate import workflow. There is no persisted
preview/confirm workflow. ZIP import and additional form fields are described by
the multipart operation schema. `--form-file NAME=PATH` reads a JSON object/array
for structured form fields; ordinary `--form` values remain literal.

## Backups and restore

Read a backup and its available languages before requesting restore. Restore keeps
the existing point-in-time behavior. A target-language-only restore does not
recreate deleted source keys; include the source language when those keys must
return. Automatic backup configuration and retention keep existing semantics.
Use `api wait getBackup` or `api wait getBackupRestore` for the resulting resources.

## Git and content integrations

```sh
lingohub api call listIntegrationAuthentications --query limit=100 --json
lingohub api call listGitHubRepositories --path integrationAuthenticationId=iau_UUID --json
lingohub api call getProjectGitIntegration --path projectId=pr_UUID --include-headers --json
lingohub api call getBranchGitIntegration --path branchId=prb_UUID --include-headers --json
```

Stored provider credentials are integration authentications. Every Git and content
provider uses the same create, read, update, credential and delete operations; the
`provider` field of the create and credential bodies selects the provider's fields.
Provider discovery runs through a stored authentication. Stored settings reads are
distinct from provider discovery and synchronization requests. The operation
inventory includes provider connections, schedules, directory selection, branch
binding, explicit pull/push and content-provider configuration.

GitHub setup uses the existing app installation link and subsequent connection
discovery. Other providers retain existing credential setup. A dedicated browser
callback returning to the CLI is separate work. Provider-native discovery pages
expose their documented cursor/bounds; do not assume every discovery response uses
standard V2 1–100-item pagination.

Poll Git pull imports and push exports through their normal job resources.
`git_source` and `git_result` report observed provider outcomes; do not infer a
remote commit or pull request from a submitted job alone.

## Membership, invitations and API keys

```sh
lingohub api call listMembers --query limit=100 --json
lingohub api call listStackMembers --path stackId=stack_UUID --query limit=100 --json
lingohub api call getWorkspaceApiKeyPermissions --path apiKeyId=api_UUID --json
lingohub api call getWorkspaceApiKeyStackPermissions --path apiKeyId=api_UUID \
  --path stackId=stack_UUID --include-headers --json
```

Stack-member lists describe explicit assignments; inherited access may not appear
there. Use effective-access operations when inspecting the resulting permissions.
Workspace keys can send invitations with the existing management permission.
Invitation attribution uses `inviter_actor_id`. Owner changes stay outside the API.

`setWorkspaceApiKeyStackPermissions` creates or replaces a key's levels on one stack;
it returns no ETag, so read the stack permissions for a current validator.
API-key permission writes accept `NO_ACCESS`, `VIEW` and `MANAGE`. Responses can
contain `WRITE` for the existing Quick Translation setting; it remains unavailable
to API keys and permission replacement preserves that unrelated configuration.
Key creation/rotation can return secrets: handle their stdout accordingly.

## Other capabilities

[The operation inventory](operations.md) includes project settings and moves,
LINA, glossary/terms, style guides, webhooks/deliveries, tasks, context images,
SDK captures and CDN distributions/releases. Use `api call` for JSON operations
and `api upload` for the three multipart operations. For example:

```sh
lingohub api download-distribution-release rel_UUID --output release.zip --json
lingohub api mcp-tools --operation setProjectFormatSettings
```

Named translation memories and their CLI group are deferred. Issues/comments await
the separate shared Issues feature. Neither is a hidden command in this version.
