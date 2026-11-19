# Windows packaging

- Build: `cargo build --release -p tonearm` with the MSVC toolchain (`x86_64-pc-windows-msvc`).
- Release builds have no console window (`windows_subsystem = "windows"` in `crates/app/src/main.rs`).
- The icon (`assets/icon.ico`) is embedded by `crates/app/build.rs` via `winresource`.
- CI publishes `tonearm-windows-x86_64.zip`.
- MSI: install WiX Toolset v3.14 and `cargo install cargo-wix`, then from the workspace root run
  `cargo wix crates\app\Cargo.toml --nocapture`. The source is `crates/app/wix/main.wxs`
  (per-machine install to Program Files, Start menu shortcut, upgrades replace the old version).
  The installer is in `target\wix\`. Not yet built on real Windows: check the paths on first run.
