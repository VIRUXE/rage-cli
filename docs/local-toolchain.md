# Testing the local toolchain

Place the checkouts alongside each other:

```text
tools/
  rage-cli/
  rage-formats/
  rage-render/
  rpf-archive-rs/
```

From PowerShell in rage-cli:

```powershell
./scripts/with-local-toolchain.ps1 test --quiet
./scripts/with-local-toolchain.ps1 tree -i rage-formats
./scripts/with-local-toolchain.ps1 -ToolchainRoot C:/dev/gta/v/tools -- test --test img
./scripts/with-local-toolchain.ps1 test --test gtasa '--' --nocapture
```

The wrapper passes Cargo arguments directly, patches both registry and renderer
git sources to the local checkouts, forwards the exit code, deletes its temporary
configuration and restores the caller's Cargo.lock even on failure. Ordinary
builds continue using published/pinned dependencies. `--locked` can reject local
patches if the original lockfile has not resolved those sources; omit it for
local toolchain work. Avoid concurrent commands that modify Cargo.lock while
the wrapper is running.

An opt-in San Andreas test run uses `GTASA_PATH`; keep the install read-only and
write generated fixtures under `target/gtasa`. Neither extracts nor converted
game files belong in commits.
