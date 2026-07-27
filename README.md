# twelf 💦

A Rust image viewer with direct NAS access.

## Features

- Local folder browsing with a collapsible directory tree. The tree tracks the folder live: files added, removed, or renamed outside the app show up immediately.
- Remote browsing over SFTP — connect to a host, browse a remote root, and load images straight from the server. The NAS must have SFTP enabled. Expanded remote folders are re-listed every 30 seconds so outside changes appear automatically; right-click → Refresh forces one immediately.
- Supported image formats: JPEG, PNG, GIF, BMP, WebP, HEIC.
- Supported video formats: MP4, M4V, MKV, WebM, MOV, AVI, WMV, FLV, MPG, MPEG, TS.
- Video playback for local and remote files: plays on selection (looping, scaled to fit, no audio), with an on-screen play/pause control, `Space` to toggle, and a draggable seek bar. Remote videos stream over SFTP and start playing before the whole file has downloaded.
- Keyboard navigation: arrow keys move to the previous/next image; the sidebar tracks and scrolls to the selection.
- Zoom with `Ctrl` + mouse wheel on the central image.
- Favorites: save a host plus a root folder and restore it with one click instead of retyping the path. Save the connect dialog's current fields with "Save current", or right-click any remote folder while browsing and choose "Add to Favorites" to save the folder you are looking at.
- SSH connection details and favorites persist in `~/.config/twelf/config.toml`. Each favorite is a `[[favorites]]` table; edit its `label` to rename it.

## Dependencies

- `libheif` — required on the system (used by `libheif-rs` for HEIC decoding).
- `ffmpeg` — development libraries required on the system (used by `ffmpeg-next` for video decoding); building also needs `clang`/`libclang` for binding generation.

## Build and run

```
cargo run --release
```

## License

Licensed under the MIT License — see [LICENSE](LICENSE).
