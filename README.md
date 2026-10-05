# anatoly

A Rust CLI code assistant (rig-core + OpenRouter) with three tools: `think`,
`read-file`, and `shell`.

Commands run inside an isolated, long-lived container instead of on the host.
At session start anatoly makes a **disposable `git clone`** of the repository
into a sibling sandbox directory, checks out a session branch
`anatoly/<timestamp>`, and starts one container that bind-mounts *only* that
sandbox directory (read-write, at its own absolute path).

```
host                                         container (podman, "sleep infinity")
├── main repo  /Users/…/anatoly              (never mounted)
├── sandbox    /Users/…/anatoly-sandbox-<ts> ── single rw mount, same path ──► cwd
└── anatoly CLI (host env: API key)          env: HOME=/tmp, git identity,
                                                 DEBIAN_FRONTEND, CARGO_TARGET_DIR
```

- The LLM calls and `OPENROUTER_API_KEY` never leave the host.
- `read-file` is a host-side tool, re-rooted at the sandbox directory.
- `shell` = `podman exec` into the container (`bash -c <cmd>`); exit codes and
  output are propagated back to the model.
- The container runs with `--network none`, `--read-only`, a `/tmp` tmpfs, and
  only explicitly listed environment variables.

## Setup

Build the sandbox image (works with podman or docker):

```sh
make sandbox-image                 # podman build -f sandbox/Dockerfile -t anatoly-sandbox:0.1 .
make sandbox-image RUNTIME=docker  # docker build ...
```

On macOS you also need the podman machine running:

```sh
podman machine start
```

Set `OPENROUTER_API_KEY` and `OPENROUTER_MODEL_NAME`, then run:

```sh
cargo run
```

## Configuration

| Env | Default | Purpose |
|---|---|---|
| `ANATOLY_RUNTIME` | `auto` (podman → docker) | container CLI to use |
| `ANATOLY_SANDBOX_IMAGE` | `anatoly-sandbox:0.1` | image to run |
| `ANATOLY_SANDBOX_DIR` | sibling `<root>-sandbox-<ts>` | clone location |
| `ANATOLY_SANDBOX_MEMORY` | `4g` | container memory cap |
| `ANATOLY_SANDBOX_CPUS` | `4` | container CPU cap |
| `ANATOLY_SHELL_TIMEOUT` | `300` | per-exec wall-clock seconds |
| `ANATOLY_GIT_AUTHOR_NAME` | host `git config user.name` | commit attribution |
| `ANATOLY_GIT_AUTHOR_EMAIL` | host `git config user.email` | commit attribution |

## Consolidating a session

The sandbox clone, its branch, and its commit history are **never deleted
automatically**. On Ctrl-C (or Ctrl-D) only the container is removed and
anatoly prints the commands to review and merge, e.g.:

```sh
git fetch /Users/…/anatoly-sandbox-<ts> 'refs/heads/*:refs/remotes/anatoly/*'
git log   <main-branch>..anatoly/<ts>   # review; `git diff` likewise
git merge --no-ff anatoly/<ts>          # or rebase / cherry-pick
rm -rf /Users/…/anatoly-sandbox-<ts>    # when done
```

If the process is `kill -9`'d the container may be left behind; the next start
removes stray containers labelled with the repository path. Run **one session
per repository at a time** — a new session removes any container labelled with
that repository.

## Fallback mode

Outside a git repository, anatoly skips the clone and branch: the container
mounts the current directory instead, and no consolidation step is printed.

## Development

```sh
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test
```

Tests that need a container runtime probe it first and skip when it (or the
sandbox image) is unavailable, so the suite still passes on a machine without a
running podman machine.
