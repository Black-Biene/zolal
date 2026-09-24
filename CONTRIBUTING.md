# Contributing to Zolal

Thanks for helping make Zolal better. Bug reports, fixes, new carrier formats, docs and UI improvements
are all welcome.

## Reporting a problem

Open an [issue](https://github.com/Black-Biene/zolal/issues) with what you did, what you expected and
what happened. Attach a sample file if you can, but **never one that holds anything real**: make a fresh
one with a throwaway passphrase.

**Security problems** (anything that could expose hidden data or the passphrase) should not go in a public
issue. Email hi@blackbiene.dev instead, and we'll reply and credit you once it's fixed.

## Sending a pull request

1. Fork the repo and create a branch from `main`.
2. Make your change. Keep it focused: one fix or feature per pull request is easier to review.
3. Run the checks before you push:

   ```bash
   cargo fmt --all
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo deny check
   ```

4. Open the pull request and say what it changes and why. Link the issue if there is one.

Ground rules that keep the engine safe to use:

- **Stream, don't buffer.** Work from paths and readers; memory must not grow with file size.
- **Stay markerless.** Nothing in the hidden data may identify it as Zolal's in the clear.
- **Permissive dependencies only.** `cargo deny check` enforces the allow-list in `deny.toml`.

## Licence of contributions

Zolal is licensed under [Apache-2.0](LICENSE). By sending a pull request you agree that your
contribution is licensed under the same terms (Apache-2.0, section 5). You keep the copyright to your work.
