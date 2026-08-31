# Contributing

Thanks for helping improve Agent Monitor. The project is currently focused on
read-only monitoring of local Codex tasks on macOS.

## Development

```bash
pnpm install
pnpm typecheck
pnpm build
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo test --all-targets
cd src-tauri && cargo clippy --all-targets -- -D warnings
```

Please keep provider-specific parsing in the adapter/core layer, avoid storing
prompts or command output, and preserve the distinction between running,
needs-action, idle, succeeded, failed, and interrupted states.

## Pull Requests

- Describe the user-visible behavior and the affected data boundary.
- Include a focused regression test for lifecycle, progress, or notification changes.
- Do not commit build output, local databases, credentials, or personal Codex session data.
