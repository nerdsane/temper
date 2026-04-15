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
- **StreamUpdated**: Fired by $value PUT handler after upload succeeds; advances to Ready
- **Lock / Unlock**: Prevent or re-enable content modifications
- **Archive**: Terminal state

**Invariant**: Files in Ready or Locked state always have content.

### FileVersion

Immutable record of a specific file version. Created on each content upload.

**States**: Current → Superseded

**Key actions**:
- **Create**: Record version with file ID, version number, content hash, size, and creator
- **Supersede**: Mark as replaced by a newer version (terminal)

## Setup

```
temper.install_app("<tenant>", "temper-fs")
```
