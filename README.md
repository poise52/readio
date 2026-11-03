# Readio

English | [Русский](README.ru.md)

Readio is a desktop EPUB, FB2 and TXT reader with speech synthesis. Books, settings, bookmarks and reading positions are stored in SQLite in the application data directory. The application interface is currently in Russian.

## Install

Download an installer from [GitHub Releases](https://github.com/poise52/readio/releases). Builds target Windows 11 x64 and macOS on Apple Silicon. The Windows installer is not signed with a publisher certificate. The macOS build is ad-hoc signed and not notarized by Apple; Gatekeeper may require you to allow it in Privacy & Security on first launch.

## Speech engines

- **OpenRouter:** create an [API key](https://openrouter.ai/settings/keys), save it in Readio settings, and select a TTS model and, where available, a voice. The key is stored in macOS Keychain or Windows Credential Manager, not in SQLite or book settings. The catalog includes paid and free models; availability and prices depend on OpenRouter and your account.
- **Edge:** online speech with built-in Dmitry and Svetlana voices. Voice preview uses the selected engine.
- **Piper:** install Piper separately along with an ONNX voice model and its `.onnx.json` configuration. Put the executable in `piper/` under Readio's data directory or add it to `PATH`, and put models in `models/piper/`. Alternatively set `READIO_PIPER_BIN` and `READIO_PIPER_MODELS` to their respective paths. Model language is read from its configuration.

Configure audio format and text limits per OpenRouter model according to the provider's documentation. Readio does not infer provider limits from model IDs. PCM output requires explicit sample rate and channel count; mono PCM16 is supported.

## Offline listening and costs

Before downloading a book, Readio displays the voice and an estimate for remaining blocks and asks for confirmation. OpenRouter does not specify the billing unit in its model pricing API, so paid-model estimates remain unknown until you select a unit based on the provider's documentation. Estimates use that unit and API rates, are not actual charges, and exclude unknown extra fees.

You can set a limit on new requests and a USD budget for each download or resume. Cached blocks do not consume the request limit. Unknown costs require separate confirmation and a new-request limit. The budget is checked before each request; a rate change can cause an already submitted request to exceed it. Actual expenses are tracked separately when the provider reports them; **Reconcile costs** checks previous attempts without generating speech again.

Downloaded audio plays without a network connection or API key and keeps the model, voice and dictionary selected at download time. Unfinished downloads can be resumed manually after restarting, with a fresh estimate. Pausing does not cancel a request already sent; its completed audio is cached. Removing an offline edition unpins its blocks. Temporary cache is limited to 1 GiB; pinned editions are excluded. Position within a block is preserved between launches. Valid Edge timing data supports text highlighting and navigation; without suitable timing data, text navigation is approximate.

Generation attempts are recorded before OpenRouter requests are sent. If a request's outcome is unknown after a crash, existing valid cached audio is reused; otherwise retrying requires explicit consent because another charge is possible. Provider idempotency and single billing after a connection failure are not guaranteed. On exit, Readio stops accepting new generation requests and waits for submitted blocks to be saved. Only one Readio process can use a data directory at a time.

Back up the entire application data directory, including `audio-cache/editions`, `manifests` and `blocks`. A damaged edition manifest does not block other books; the diagnostic banner can restore it from a JSON backup after checking its identity. This restores the description and pins, not deleted audio or a missing plan file. Automatic audio eviction is suspended while references from a damaged manifest are unknown.

Import supports UTF-8, UTF-16 with BOM and declared XML encodings including Windows-1251. Unknown encodings are not silently substituted. Batch imports are transactional: an error prevents a partial import.

## Development and builds

Install Rust 1.99.1, Node.js 24.15.0 and the [Tauri v2 system prerequisites](https://v2.tauri.app/start/prerequisites/), then run:

```sh
npm --prefix frontend ci
npm --prefix frontend run tauri -- dev
```

Checks:

```sh
cargo test --workspace
npm --prefix frontend run typecheck
npm --prefix frontend test
```

On Windows, install MSVC C++ Build Tools and make NASM available in `PATH`. Build the Windows x64 installer on Windows:

```sh
npm --prefix frontend run tauri -- build --target x86_64-pc-windows-msvc
```

On an Apple Silicon Mac, use `npm --prefix frontend run tauri -- build`.

The `CI` GitHub Actions workflow runs on pushes and pull requests and can also be run manually. Its artifacts contain a Windows x64 NSIS installer and macOS Apple Silicon DMG and application ZIP. CI does not publish releases.

The `Release` workflow publishes installers for a pushed `v*` tag. To run it for an existing tag, use **Actions → Release → Run workflow** and enter, for example, `v0.2.5`. The tag must already exist and contain the application, build files and release notes at `docs/releases/<tag>.md`; its version must match the Cargo, Tauri and frontend versions. See the [v0.2.5 release notes](docs/releases/v0.2.5.md). Prerelease tags produce GitHub prereleases. The workflow uses the automatically provided `GITHUB_TOKEN` with `contents: write`; it needs no separate secrets or `.env`. Code checks run in the separate `CI` workflow. The signing limitations above apply to release builds.
