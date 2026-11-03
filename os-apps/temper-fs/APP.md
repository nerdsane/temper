# Temper FS

Versioned virtual filesystem with workspaces, directories, files, and file versions. Provides quota-managed storage for agent workspaces, conversation logs, soul/skill content, and any file-based artifacts. Content is stored externally and tracked via content hashes.

## Entity Types

### Workspace

Top-level container for TemperFS. Manages storage quota and usage tracking across all files.

**States**: Active → Frozen → Archived

**Key actions**:
- **Create**: Initialize with name and storage quota
- **UpdateQuota**: Adjust the storage limit
- **IncrementUsage / DecrementUsage**: Track storage consumption
- **IncrementFileCount / DecrementFileCount**: Track file count
- **Freeze / Thaw**: Toggle read-only mode
- **Archive**: Terminal state

**Invariant**: Usage cannot exceed quota while Active.

### Directory

Hierarchical container within a workspace. Tracks child items.

**States**: Active → Archived

**Key actions**:
- **Create**: Create at a given path within a workspace
- **AddChild / RemoveChild**: Manage contained files and subdirectories
- **Rename**: Rename and update path
- **Archive**: Only allowed when empty (terminal)

### File

Single file within TemperFS. Content stored externally; metadata tracked here.

**States**: Created → Ready → Locked → Archived

**Key actions**:
- **Create**: Create file entry (content not yet uploaded)
- **StreamUpdated**: Fired by $value PUT handler after upload succeeds; advances to Ready and spawns a fresh immutable `FileVersion`
- **Lock / Unlock**: Prevent or re-enable content modifications
- **Archive**: Terminal state

**Invariant**: Files in Ready or Locked state always have content.

### FileVersion

Immutable record of a specific file version. Created on each content upload and linked back to the previous immutable version before superseding it.

**States**: Current → Superseded

**Key actions**:
- **Create**: Record version with file ID, version number, content hash, mime type, size, previous version ID, and creator
- **Supersede**: Mark as replaced by a newer version (terminal)

**Belongs to its File**: a version's `file_id` must name a File that exists, and deleting a File deletes its versions
with it. `File.LastVersionId` and `PreviousVersionId` are pointers between versions of one File and carry no delete
rule.

## Setup

```
temper.install_app("<tenant>", "temper-fs")
```

## Where the bytes go

A File's bytes are kept by the tenant's blob store under `temper-fs/<content hash>`: a remote object store (R2 or S3)
when the vault has `blob_endpoint` (with `blob_bucket`, `blob_access_key`, `blob_secret_key`), otherwise the server's
own disk. The vault may also hold:

- `blob_session_token`: the session token of temporary credentials, signed with every request;
- `blob_prefix`: a folder of the bucket every key goes under (one deployment's own place in a shared bucket);
- `blob_link_access_key`, `blob_link_secret_key`, `blob_link_session_token`: read-only credentials that sign links. A
  link carries the session token of whatever signs it, and temporary credentials can be rebuilt from their token, so
  with temporary store credentials and no link credentials there are no links at all.

With a remote store a File's bytes are never copied into the database.

- **Upload**: `PUT Files('<id>')/$value` with a `Content-Length` streams: Cedar decides from the headers, the body is
  staged on disk while it is hashed, sent to the store, and only then committed. It is not bound by the router's
  buffered body limit (at most 2 GiB; eight uploads per tenant at once; no throughput floor, only two minutes without
  progress or two hours in all). A body without `Content-Length`, or any body for a local store (which keeps the
  database copy), is buffered as before. A File that takes no content in its current state is refused before its body
  is read.
- **Read**: `GET Files('<id>')/$value?redirect=1` answers, once Cedar lets the reader read the File, `302` to a
  five-minute link to its bytes in the store (`redirect=<seconds>`, 60 to 3600, for a film), labelled with its type
  (pictures, films and sounds open in place; anything else is a download). Without `redirect`, with a local store, or
  with no credentials safe to put in a link, the bytes come through the server.
