# twelf 💦

A Rust image viewer with direct NAS access.

## Features

- Local folder browsing with a collapsible directory tree.
- Remote browsing over SFTP — connect to a host, browse a remote root, and load images straight from the server. The NAS must have SFTP enabled.
- Supported formats: JPEG, PNG, GIF, BMP, WebP, HEIC.
- Keyboard navigation: arrow keys move to the previous/next image; the sidebar tracks and scrolls to the selection.
- Zoom with `Ctrl` + mouse wheel on the central image.
- SSH connection details persist in `~/.config/twelf/config.toml`.

## Dependencies

- `libheif` — required on the system (used by `libheif-rs` for HEIC decoding).

## Build and run

```
cargo run --release
```

## License

Licensed under the MIT License — see [LICENSE](LICENSE).
