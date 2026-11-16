# V2 workflow cookbook

Use public IDs from API responses in place of the placeholders below. These examples
are command recipes, not evidence of live provider testing. Read the operation's
schema and description in [the bundled OpenAPI contract](../contract/v2.json), or
inspect its input with `lingohub api mcp-tools --operation OPERATION`.

## Shared content and keys

A segment is shared content. Its keys are where it occurs in files, and its texts
hold the content of each language. The texts are part of the segment: there are no
separate text resources.

```sh
lingohub api call getSegment --path segmentId=seg_UUID --include-headers --json
lingohub api call getSegment --path segmentId=seg_UUID --query language_codes=en,de --json
lingohub api call listSegments --path branchId=prb_UUID --query limit=20 --json
lingohub api call listSegmentKeys --path segmentId=seg_UUID --json
lingohub api call updateSegment --path segmentId=seg_UUID \
  --file segment-patch.json --if-match '"SEGMENT_ETAG"' --json
```

A segment PATCH is a JSON merge-patch. `texts` maps a language code to that
language's new text and names only the languages that change:

```json
{"texts":{"de":{"variants":[{"device":"OTHER","plural_form":"OTHER","content":"Anmelden"}]}}}
```

`variants` replaces the complete text of that language (`device`, `plural_form`,
`content`); `variants: []` clears it. Shared text edits affect every key of the
segment. The placeholder settings and automations of the segment's oldest key's
project apply unless `--query project_id=pr_UUID` names another project.

Change the status of one text through its segment:

```sh
lingohub api call transitionSegment --path segmentId=seg_UUID \
  --field 'language_code="de"' --field 'action="APPROVE"' \
  --if-match '"SEGMENT_ETAG"' --json
```

`action` is `APPROVE`, `REOPEN`, `MARK_TRANSLATED` or `REJECT`.

A key is renamed with a PATCH of the key itself:

```sh
lingohub api call getKey --path keyId=ress_UUID --include-headers --json
lingohub api call updateKey --path keyId=ress_UUID \
  --file rename.json --if-match '"KEY_ETAG"' --json
```

`key` is the full key as `getKey` returns it; `group_key` sets the group in formats
with one group, and null removes it. `is_rename_allowed` and `is_delete_allowed` in
`getKey` say whether the project and format permit the change: single-language CSV,
document, subtitle, Storyblok and integration projects do not. Renaming and deletion
use the key's validator; a rename returns the new one. Use files or stdin for large
escaped keys; normalization and key limits are enforced by the backend.

## Create keys and change segments in batches

```sh
lingohub api call createKey --path resourceId=rc_UUID \
  --file new-key.json --idempotency-key create-sign-in --include-headers --json
lingohub api call createKeysBulk --path resourceId=rc_UUID \
  --file keys.json --idempotency-key create-navigation --json
lingohub api call updateSegmentsBulk --path branchId=prb_UUID --file segments.json --json
lingohub api call transitionSegmentsBulk --path branchId=prb_UUID --file transitions.json --json
```

Each created key gets a fresh shared segment. A single key body contains `key`
and optionally `is_plural`, `group_key`, `label_ids` and `source_variants`:

```json
{"key":"sign_in","is_plural":false,"source_variants":[{"device":"OTHER","plural_form":"OTHER","content":"Sign in"}]}
```

A batch body is `{"items":[...]}` with up to 150 items. `createKeysBulk` items are
key bodies; `updateSegmentsBulk` items are segment patches with a `segment_id`;
`transitionSegmentsBulk` items carry `segment_id`, `language_code` and `action`:

```json
{"items":[{"segment_id":"seg_UUID","language_code":"de","action":"APPROVE"}]}
```

Batches are atomic. Either every item is applied, and the response lists the
created keys or changed segments in request order, or none is: the first problem
stops the batch and its error points at the item (for example `#/items/1/key`).
The CLI exits 1 when a success response does not list one resource per submitted
item. Requests are never retried; send an `--idempotency-key` to make a
deliberate retry safe.

## Import and export

```sh
lingohub api upload createImport --path branchId=prb_UUID \
  --upload ./de.json --form language_code=de --include-headers --json
lingohub api wait getImport --path importId=rin_UUID --json
lingohub api call listImportEntries --path importId=rin_UUID --json
lingohub api call listImportChanges --path importId=rin_UUID --json
lingohub api call getExport --path exportId=reg_UUID --include-headers --json
lingohub api wait getExport --path exportId=reg_UUID --json
lingohub api download-export reg_UUID --output export.zip --json
lingohub api download-export-entry reg_UUID ree_UUID --output de.json --json
```

`listImportChanges` lists what an import changed, one item per segment and file:
the key change and the text changes in each language. Imports apply the existing
immediate import workflow. There is no persisted
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
lingohub api call updateProjectGitIntegration --path projectId=pr_UUID \
  --file git-patch.json --if-match '"INTEGRATION_ETAG"' --json
lingohub api call syncStoryblokIntegration --path projectId=pr_UUID \
  --field is_import_new_resources=true --json
```

Stored provider credentials are integration authentications. Every Git and content
provider uses the same create, read, update, credential and delete operations; the
`provider` field of the create and credential bodies selects the provider's fields.
Provider discovery runs through a stored authentication. An integration is one
resource: read it with `getProject<Provider>Integration` and change it with
`updateProject<Provider>Integration`, a JSON merge-patch. For Git the patch can name
the connection, `sync` (schedules), `push` and `selection` (branching and filters);
content providers take `path_filters`, HubSpot also `enabled_content_types`.
Selection changes have side effects such as starting imports or removing branches;
read the operation description first. `sync<Provider>Integration`,
`pull<Provider>Integration` and `push<Provider>Integration` start jobs explicitly.

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
lingohub api call getStackMemberPermissions --path stackId=stack_UUID \
  --path memberId=wsu_UUID --json
lingohub api call listWorkspaceApiKeys --query limit=100 --json
lingohub api call getWorkspaceApiKey --path apiKeyId=api_UUID --include-headers --json
lingohub api call updateWorkspaceApiKey --path apiKeyId=api_UUID \
  --field 'name="CI key"' --if-match '"KEY_ETAG"' --json
```

Stack-member lists describe explicit assignments; inherited access may not appear
there. `getStackMemberPermissions` shows what a member can actually do in a stack.
Workspace keys can send invitations with the existing management permission.
`inviter_email` names who sent an invitation. Owner changes stay outside the API.

The API lists workspace API keys and renames workspace-managed ones or changes
their description and expiry. Creating keys, rotating tokens, revoking another
key and changing a key's permissions are done in the LingoHub app.
`lingohub api call deleteApiKey` revokes only the key that makes the request.

## Project settings

All settings of a project are one document with the sections `export`, `import`,
`format`, `keys`, `placeholders` and, for integration projects, `integrations`:

```sh
lingohub api call getProjectSettings --path projectId=pr_UUID --include-headers --json
lingohub api call updateProjectSettings --path projectId=pr_UUID \
  --file settings-patch.json --if-match '"SETTINGS_ETAG"' --json
```

The PATCH is a JSON merge-patch: send only the sections and fields that change;
lists are replaced as a whole. All changes apply together or not at all.

```json
{"keys":{"normalization_strategy":"ALL"},"placeholders":{"definitions":["DEFAULT_ICU","IOS_PRINTF"]}}
```

## Other capabilities

[The operation inventory](operations.md) includes project moves, LINA,
glossary/terms, style guides, webhooks/deliveries, context images and CDN
distributions/releases (read-only). Use `api call` for JSON operations and
`api upload` for the two multipart operations. For example:

```sh
lingohub api call getProjectCapabilities --path projectId=pr_UUID --json
lingohub api download-distribution-release rel_UUID --output release.zip --json
lingohub api mcp-tools --operation updateProjectSettings
```

Removing a project language keeps its stack language unless
`--query delete_unused_stack_language=true` is passed to `deleteProjectLanguage`.

Named translation memories and their CLI group are deferred. Issues/comments await
the separate shared Issues feature. Neither is a hidden command in this version.
