# Changelog

Notable changes per release, one line each. Newest first.

## v0.9.6

- Compare Directories can now compare by content with Ctrl+Shift+F2, catching a file that differs without differing in size or date. Shift+F2 stays the quick size-and-date compare.

## v0.9.5

- A git-state column in the local listing: modified, staged, added and untracked flags, shown only inside a repository.
- Browse a directory's git history as a folder tree; open, view and diff any file at any commit.
- Warn before F4 opens a file larger than the configured limit.

## v0.9.4

- S3 backend: browse buckets, view keys, upload and download, bucket to bucket.
- WebDAV backend: browse, view, upload and download.
- Checksums: create and verify SHA-256 and CRC32 sidecars.
- Split and merge files into and from numbered parts.
- Create symlinks and hardlinks, and edit permissions.
- Bookmarks for the network protocols, with a password field and optional AWS environment variables.
- S3 speaks plain HTTP where asked, signs only when it has a key, and surfaces the endpoint's own error text.

## v0.9.3

- See what changed in the viewer: a diff mode offered alongside the file's own format.
- Contextual help pops up full screen on top, no longer squeezed into a dialog corner.
- The viewer's help page is generated from the keymap like every other page.

## v0.9.2

- The npm installer installs the latest release, and its version is kept in step with the crate.

## v0.9.1

- Viewer mode 3 searches the document it is drawing.
- Enter a container by what it is, not by what it is named.

## v0.9.0

- First public 0.9 release: the two-panel manager, viewer, archives, disk images, SSH/SFTP and SMB, the job engine, and the ten packaged targets.
