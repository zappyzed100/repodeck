# Third-Party Notices

RepoDeck (`repodeck.exe`, `repodeck-hook.exe`) is licensed under the MIT
License (see `LICENSE`). It links against the following third-party
components. This list covers direct dependencies declared in `Cargo.toml`
and must be regenerated whenever dependencies change (for example with
`cargo about` before cutting a release, to also capture transitive
dependencies).

## Slint

RepoDeck's UI is built with [Slint](https://slint.rs). Slint is used here
under the Slint Royalty-free Desktop License
(`LicenseRef-Slint-Royalty-free-2.0`, part of Slint's dual/triple licensing
alongside GPL-3.0-only and the commercial `LicenseRef-Slint-Software-3.0`).
See <https://github.com/slint-ui/slint/blob/master/LICENSES> for full license
texts. Slint itself is not redistributed; only the RepoDeck binaries that
link against it are distributed.

## Rust crates

| Crate | License |
|---|---|
| slint | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 |
| slint-build | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 |
| serde | MIT OR Apache-2.0 |
| serde_json | MIT OR Apache-2.0 |
| uuid | Apache-2.0 OR MIT |
| regex | MIT OR Apache-2.0 |
| thiserror | MIT OR Apache-2.0 |
| anyhow | MIT OR Apache-2.0 |
| tracing | MIT |
| tracing-subscriber | MIT |
| tracing-appender | MIT |
| parking_lot | MIT OR Apache-2.0 |
| raw-window-handle | MIT OR Apache-2.0 OR Zlib |
| time | MIT OR Apache-2.0 |
| windows | MIT OR Apache-2.0 |
| embed-manifest | MIT |
| resvg (build only) | Apache-2.0 OR MIT |
| ico (build only) | MIT |
| winresource (build only) | MIT |
| tempfile (dev/build only) | MIT OR Apache-2.0 |
| pretty_assertions (dev only) | MIT |

Full license texts for MIT and Apache-2.0 are available from the respective
crate repositories linked on <https://crates.io>.
