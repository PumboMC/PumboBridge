# Contributing to PumboBridge

Thanks for taking the time. Bug reports, fixes, tests, translations and docs are all welcome. This page explains how to build the plugin, run the tests and send a pull request.

Found a security problem? Don't open an issue, see [SECURITY.md](SECURITY.md).

## What you need

- **Rust stable** with the WebAssembly target: `rustup target add wasm32-wasip2`
- **git** that can reach GitHub: Cargo fetches the shared Pumbo libraries (`pumbo-common`, `pumbo-sdk`, `pumbo-contracts`, `pumbo-bridge-proto`) from [PumboProx](https://github.com/PumboMC/PumboProx) with your git command line (see `.cargo/config.toml`)
- [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) for the license and source check: `cargo install cargo-deny --locked`
- To try the plugin: a [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) server, and [PumboProx](https://github.com/PumboMC/PumboProx) for the proxy build. PumboProx has a script that starts a proxy with two Pumpkin servers on your machine (`tools/dev-network.sh`, see its CONTRIBUTING.md).

## Layout

```
crates/pumbo-bridge-core        all the logic, no platform code, tested natively
plugins/pumbo-bridge-pumpkin    the plugin for a Pumpkin server
```

Rules belong in the core crate; the platform crates only connect it to Pumpkin or the proxy. A fix in the core reaches both builds.

## Building

Every folder in `plugins/` has a `build.sh` that builds its `.wasm` files into a `dist/` folder and lists them at the end; the plugin's README says which file goes where. For example:

```sh
plugins/pumbo-bridge-pumpkin/build.sh
```

## Tests and checks

Run these before you open a pull request:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
```

## Code style

- `cargo fmt --all` before you commit (`rustfmt.toml`: lines up to 120 characters).
- Clippy must be clean with `-D warnings`. The workspace denies `unwrap`, `expect`, `panic!`, `unreachable!`, indexing that can panic and `unsafe` outside tests: return an error or handle the case instead.
- Comments and docs are in English and say why, not what.
- Player-facing texts live in the language files; keep English and Polish in step when you add or change a message.
- New dependencies must pass `cargo deny check`. Say in the pull request why the dependency is needed.

## Commits and pull requests

1. Fork the repository and branch off `main`: `feat/<topic>` for something new, `fix/<topic>` for a bug, `chore/<topic>` for the rest.
2. Keep one topic per pull request.
3. Write commit messages as one plain sentence that says what the change does, starting with a capital letter and without a full stop. Prefix the part of the code when that helps, for example `PumboBridge for PumboProx: test that a broken config is refused on reload`. We don't use Conventional Commits prefixes (`feat:`, `fix:`).
4. Add or update tests for what you change and run the checks above.
5. Open the pull request against `main` and fill in the template: what changes, why, and how you tested it (with the Pumpkin, PumboProx and client versions if you tried it in the game).

Changes to the shared libraries go to [PumboProx](https://github.com/PumboMC/PumboProx).

## License of contributions

PumboBridge (the core and every build) is licensed under GPL-3.0-only. By sending a pull request you agree that your contribution is licensed under GPL-3.0-only too. There is no separate contributor agreement.

The shared libraries from PumboProx are dual-licensed under MIT OR Apache-2.0, so other plugins can use them under any license.

## Questions

Open an [issue](https://github.com/PumboMC/PumboBridge/issues) for bugs and ideas. For a bigger change, open an issue first and describe what you have in mind.
