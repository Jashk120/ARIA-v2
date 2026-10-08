# Extra — Optional Plugin Home

`daemon/Extra/` holds everything ARIA can run **without**: plugins the core
daemon discovers but never requires. The default build and the air-gapped
profile work with plugin directories absent or empty.

## Convention

Each plugin lives in its own directory:

```
daemon/Extra/<plugin-name>/
  README.md          # what it does, what it costs (network? keys? fees?)
  skills/...         # WASM skill crates, same layout as daemon/skills/...
  host/...           # optional host-side code, wired via #[path]
```

Rules:

1. **Core never imports a plugin unconditionally.** Discovery is
   directory-scan only (see `daemon/src/skills/paths.rs`); host wiring
   stays behind the skill's own `capabilities.*` flags.
2. **A plugin lists its egress.** Network, keys, fees, external
   dependencies — all in its README, so air-gap review is a read, not
   an audit.
3. **Air-gap safe by default.** With DLT plugins absent or `dlt_enabled`
   off, the daemon builds and runs fully offline.

## Current plugins

- `dlt/` — Hedera payments (direct HBAR + x402), HCS audit writes, and
  the per-task audit anchor. Moved out of the daemon core so the
  air-gapped build carries zero DLT surface. Requires `HEDERA_ACCOUNT_ID` /
  `HEDERA_PRIVATE_KEY`; every HCS write costs HBAR.
