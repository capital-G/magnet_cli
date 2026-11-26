# Magnet CLI

See the docs at <https://docs.magnet.supercollider.online/> for the full design specification of the package manager.

## Releasing

Either use `cargo install cargo-release` and `cargo release minor --execute`, or do the following steps manually:

* Bump version in `Cargo.toml`
* Build, in order to update `Cargo.lock`
* Create a commit and tag it, e.g. `git tag v0.2.0`
* Push with tags, i.e. `git push --follow-tags`

This will trigger the CI and users will be notified about a new release within the CLI.

## License

AGPL-3.0
