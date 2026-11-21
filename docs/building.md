# Building from source

The program is Rust (Tauri 2). Its window is a separate web app, built from [mycl-shrubbery](https://github.com/armistice-computing/mycl-shrubbery) at the revision named in `shrubbery-ui.lock.json`.

## You need

- Rust 1.88 or newer.
- Node 22 or newer, and pnpm.
- The Tauri command line: `cargo install tauri-cli --version "^2"`.
- git.
- On Linux, the packages Tauri needs. On Ubuntu 22.04:
  `build-essential cmake clang libclang-dev libssl-dev pkg-config libwebkit2gtk-4.1-dev librsvg2-dev libayatana-appindicator3-dev patchelf`.

## Build the app

```bash
git clone https://github.com/armistice-computing/mycl.git
cd mycl
pnpm install
pnpm build
```

`pnpm build` runs `cargo tauri build`. Before it compiles, it runs `scripts/shrubbery-frontend.mjs build`, which:

1. Reads `shrubbery-ui.lock.json`.
2. Finds the window's source at that revision. It looks in `SHRUBBERY_DIR` if you set it, then in `../mycl-shrubbery`, and otherwise fetches the revision into `src-tauri/target/shrubbery-ui/`.
3. Builds it with pnpm and copies the result to `frontend/dist`, which the program packs in.

The bundles are written under `src-tauri/target/release/bundle/`. The first build compiles about a thousand crates and takes a while.

The configured bundles are for macOS and Windows. On Linux, name the ones you want: `pnpm tauri build --bundles deb,appimage`.

The builds are not signed. On macOS a bundle you build yourself opens normally; one you download from someone else is held by Gatekeeper until you allow it.

## Work on it

```bash
pnpm dev
```

This starts the window's dev server and the program beside it. It needs a `../mycl-shrubbery` checkout at the revision in `shrubbery-ui.lock.json`; at any other revision it stops and says so.

To use a profile folder of your own, and leave your real graphs alone, set `GARDEN_PROFILE_DIR` to an empty folder before you start. Two copies of the program cannot share a profile.

## Build without a window

```bash
cd src-tauri
cargo build --no-default-features --features headless --example gardend
```

`gardend` is the same node with no window and no Tauri. The tests are built the same way.

## Names you will see

The product is Mycl, and its executable is `mycl`. Inside, the older name remains: the crate is `garden`, the bundle identifier is `dev.sophia.garden`, and the variables are `GARDEN_*`. These stay so that existing profiles and agent setups keep working.
