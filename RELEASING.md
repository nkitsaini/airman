# Release Process for airman

This document describes the steps required to publish a new release of `airman`.

---

## Pre-release Checklist

Before creating a release, ensure all local tests, lints, and flake checks pass cleanly:

```bash
# 1. Check formatting
cargo fmt --all -- --check

# 2. Run Clippy (deny warnings)
cargo clippy --all-targets --all-features -- -D warnings

# 3. Run all unit and integration tests
cargo test --all-targets --all-features

# 4. Validate Nix packaging and NixOS daemon integration check
nix flake check
```

---

## Release Steps

### 1. Bump the Version

Update the version number in `Cargo.toml`:

```toml
[package]
name = "airman"
version = "0.1.0" # -> bump to target version (e.g. 0.2.0)
```

Update `Cargo.lock` by running:

```bash
cargo check
```

### 2. Commit Version Bump

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: release v0.1.0"
git push origin main
```

### 3. Create and Push Git Tag

Create an annotated tag matching the version:

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

---

## Automated GitHub Release Pipeline

Pushing a `v*` tag automatically triggers the [Release Workflow](.github/workflows/release.yml), which:

1. Cross-compiles optimized static binaries for supported Linux targets:
   - `x86_64-unknown-linux-gnu`
   - `x86_64-unknown-linux-musl`
   - `aarch64-unknown-linux-gnu`
   - `aarch64-unknown-linux-musl`
2. Packages `.tar.gz` archives with binary, `README.md`, and `LICENSE`.
3. Computes SHA256 checksums (`.sha256`).
4. Creates a GitHub Release and attaches all binary archives and checksums.

---

## Publish to Crates.io

Once the GitHub Release succeeds, publish the crate to [crates.io](https://crates.io):

```bash
cargo publish
```

---

## Post-Release Verification

1. **Verify one-line installer**:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/nkitsaini/airman/main/install.sh | sh
   airman --version
   ```
2. **Verify Nix run**:
   ```bash
   nix run github:nkitsaini/airman -- --version
   ```
3. **Verify `cargo binstall` / `cargo install`**:
   ```bash
   cargo binstall airman
   # or
   cargo install airman
   ```
