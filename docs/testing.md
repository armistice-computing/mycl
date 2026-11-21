# Testing

## What to run before you push

```bash
cd src-tauri
cargo test --lib --no-default-features --features headless
```

This is the main suite: about two thousand tests inside the crate, built without a window. It needs no running app and no network. It makes its own profile folders in the system temp folder and removes them.

## Known failures

Two tests in the main suite fail today: `harness_fresh_parity_old_vs_new_net_state_identical` and `importance_view_quotes_valued_blocks_and_links_each_to_its_source`. Nine more fail after them in the same run, with `PoisonError`, because they share a lock the first two leave poisoned. A run with exactly these eleven failures is a clean run. Anything else is yours.

## The other kinds

| Command | What it covers |
|---|---|
| `cargo test --no-default-features --features headless` (in `src-tauri`) | The suite above, plus the tests in `src-tauri/tests/`: block and composition operations, archive and vault import, markdown round trips, web clips, Yjs wire compatibility, and tests that start a real `gardend` process. Build the node first (`cargo build --no-default-features --features headless --example gardend`); the process tests start it from `target/debug/examples/`. Add `--no-fail-fast`, or the known failures below stop the run before these start. The tests do not compile with the default (desktop) features. |
| `pnpm test:mcp-stdio` | The Node version of the agent bridge and the key tool, in `scripts/`. |
| `pnpm parity:inventory` | Checks that the generated OpenAPI file, the route inventory and the scope table in `parity/` agree with the code. |

## Fixtures

Fixtures are files under `src-tauri/tests/fixtures/` and beside the modules that use them. Tests read them with `include_str!` or from `CARGO_MANIFEST_DIR`, so they run from any checkout.

## When a test fails only sometimes

Run it alone, then with `--test-threads=1`. Several tests share process-wide settings such as `GARDEN_PROFILE_DIR`, and guard them with a lock; a new test that sets one must take the same lock.

## Before a release

Build the bundle, start it on an empty profile, and look: the first graph should be Start, with its folders and wires, and nothing should point outside it. No automated check replaces opening the app.
