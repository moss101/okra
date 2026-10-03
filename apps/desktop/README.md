# okra desktop shell

A thin Electron main (window + daemon lifecycle only) around the workbench
served by `okra serve`. MASTER-PLAN block #50: no business logic lives here.

## Run (dev)

```sh
cargo build -p okra          # or --release
npm install
npm start                    # window; spawns the daemon from target/
npm run smoke                # handshake check only, no window
npm test                     # handshake unit tests (plain node)
```

## The real model (provider config)

Without a provider config the daemon runs the offline demo sampler —
canned responses, good for smoke tests only. To drive the real thing,
create `~/.okra/desktop-provider.json`:

```json
{
  "provider": "openai",
  "model": "<your model name>",
  "apiKey": "<your key>",
  "baseUrl": "<your OpenAI-compatible endpoint>",
  "extraHeaders": "Optional: K: V; K: V"
}
```

The shell reads it at every launch and passes `--provider/--model` plus
the key/base-url into the daemon's environment. (A dev-shell launch can
instead just export `OKRA_API_KEY` + `OKRA_BASE_URL`.)

## Package (.app)

```sh
cargo build --release -p okra   # the binary bundled into the .app
npm run package                 # electron-packager + ad-hoc re-sign
```

Output: `release/okra-desktop-darwin-arm64/okra-desktop.app` with the
okra binary inside (`Contents/Resources/okra`). The shell resolves it
from the bundle first, then falls back to the repo's `target/` builds.

Smoke the packaged app (packaged Electron has no stdout; the report goes
to a file):

```sh
OKRA_SMOKE=1 OKRA_SMOKE_OUT=/tmp/smoke.json \
  release/okra-desktop-darwin-arm64/okra-desktop.app/Contents/MacOS/okra-desktop
cat /tmp/smoke.json
```

Note: the agent harness sets `ELECTRON_RUN_AS_NODE=1`, which makes the
Electron binary run as plain node — launch with
`env -u ELECTRON_RUN_AS_NODE` to get the real app in that environment.
