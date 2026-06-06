# twelf 💦

A Rust image viewer with direct NAS access.

## Features

- Local folder browsing with a collapsible directory tree.
- Remote browsing over SFTP — connect to a host, browse a remote root, and load images straight from the server. The NAS must have SFTP enabled.
- Supported image formats: JPEG, PNG, GIF, BMP, WebP, HEIC.
- Supported video formats: MP4, M4V, MKV, WebM, MOV, AVI, WMV, FLV, MPG, MPEG, TS.
- Video playback for local and remote files: plays on selection (looping, scaled to fit, no audio), with an on-screen play/pause control, `Space` to toggle, and a draggable seek bar. Remote videos stream over SFTP and start playing before the whole file has downloaded.
- Keyboard navigation: arrow keys move to the previous/next image; the sidebar tracks and scrolls to the selection.
- Zoom with `Ctrl` + mouse wheel on the central image.
- SSH connection details persist in `~/.config/twelf/config.toml`.

## Dependencies

- `libheif` — required on the system (used by `libheif-rs` for HEIC decoding).
- `ffmpeg` — development libraries required on the system (used by `ffmpeg-next` for video decoding); building also needs `clang`/`libclang` for binding generation.

## Build and run

```
cargo run --release
```

## License

Licensed under the MIT License — see [LICENSE](LICENSE).
