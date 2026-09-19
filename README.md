# twelf 💦

A Rust image viewer with direct NAS access.

## Features

- Local folder browsing with a collapsible directory tree. The tree follows the folder live through the OS file watcher: files added, removed, or renamed outside the app show up immediately, a selected file that is renamed stays selected, and an image overwritten in place is reloaded. If the watch cannot be set up (the inotify watch limit on a very large tree) the status bar says so, and changes made by another machine on a network mount are not reported by the OS at all — in both cases right-click a folder → Refresh re-lists it.
- Remote browsing over SFTP — connect to a host, browse a remote root, and load images straight from the server. The NAS must have SFTP enabled. Expanded remote folders are re-listed every 30 seconds so outside changes appear automatically; right-click → Refresh forces one immediately.
- Supported image formats: JPEG, PNG, GIF, BMP, WebP, HEIC. EXIF orientation is applied, so portrait photos display upright.
- Supported video formats: MP4, M4V, MKV, WebM, MOV, AVI, WMV, FLV, MPG, MPEG, TS.
- Video playback for local and remote files: plays on selection (looping, scaled to fit, no audio), with an on-screen play/pause control, `Space` to toggle, and a draggable seek bar. Remote videos stream over SFTP and start playing before the whole file has downloaded.
- Keyboard navigation: arrow keys move to the previous/next image; the sidebar tracks and scrolls to the selection. `Home` scrolls the sidebar back to the root.
- Search with `Ctrl` + `F`: matches file and folder names (case-insensitive) under the current root, local or remote; a folder whose name matches keeps its whole contents. While results are shown the arrow keys step through them. `Esc` closes the search.
- Select several files at once: `Ctrl` + click adds a file to the selection or takes it out, `Shift` + click selects the run from the last file clicked, along the rows as listed (the tree, or search results). Works in both trees; folders are not part of it.
- Right-click a file or folder to rename or delete it (delete asks first), in either tree. Delete on a file that is part of a multiple selection deletes the whole selection; on any other row it deletes that row alone. On the remote tree, Download copies a file to a location you pick, or a whole folder into a directory you pick — local files that already exist are left alone and counted as skipped.
- Upload: right-click a remote folder → Upload, pick local files, and they are copied into that folder, with progress and a cancel button in the status bar. A file whose name is already taken on the server is left alone and counted as skipped; nothing is overwritten.
- Zoom with `Ctrl` + mouse wheel on the central image.
- Favorites: save a host plus a root folder and restore it with one click instead of retyping the path. Save the connect dialog's current fields with "Save current", or right-click any remote folder while browsing and choose "Add to Favorites" to save the folder you are looking at.
- SSH connection details and favorites persist in `~/.config/twelf/config.toml`. Each favorite is a `[[favorites]]` table; edit its `label` to rename it. A file that no longer parses is never overwritten: the app says so at startup, runs on defaults, and renames the file to `config.toml.bad` before it next saves.

## Connecting

- Authentication is by private key file only (RSA, ECDSA, Ed25519). Passphrase-protected keys are not supported yet.
- The server's host key is verified. A host already in `~/.ssh/known_hosts` connects without a question. An unknown host brings up its SHA-256 fingerprint — compare it with `ssh-keygen -lf` on the server's host key — and trusting it records the key in twelf's own list, `~/.config/twelf/known_hosts`; `~/.ssh/known_hosts` is only ever read. A host whose key differs from the recorded one is refused, with the file and line to remove if the change is expected.
- A connection attempt gives up after 20 seconds and can be cancelled from the menu bar. The session you are on stays usable until the new one succeeds, so a mistyped host costs nothing.
- An idle session is kept alive with a ping every 15 seconds. When a session ends — the server restarts, the link dies — the menu bar reports it and the remote tree closes; reconnect from File → Connect SSH.

## Disk cache

Remote images are cached on disk under `~/.cache/twelf/keys/`, one directory per SSH key, so revisiting a folder does not download it again. Each holds up to 4 GiB and drops the least recently used files beyond that; a cached file is reused only while the remote file's size and modification time still match.

**The cached image files are stored unencrypted.** Only the index (which remote path each file belongs to) is encrypted, with a key derived from the SSH key file, and the directories are readable by your user only. Anyone who can read your home directory can see the images you have viewed. Cache → Clear Cache deletes all of it, for every key.

## Dependencies

- A Rust toolchain of 1.88 or newer.
- `libheif` — required on the system (used by `libheif-rs` for HEIC decoding).
- `ffmpeg` — development libraries required on the system (used by `ffmpeg-next` for video decoding); building also needs `clang`/`libclang` for binding generation.
- OpenSSL development libraries (`libcrypto`) — the disk cache's index uses SQLCipher, which is built from source but links the system's libcrypto.

Developed and tested on Linux. Following files renamed or rewritten outside the app relies on inotify; other platforms' watchers report less.

## Build and run

```
cargo run --release
```

## License

Licensed under the MIT License — see [LICENSE](LICENSE).
