# Lock file `magnet.lock`

Magnet will also create a lock file which stores the exact version of every installed dependency.
This is particularly relevant for transient dependencies, and only with a lock file a full replication of the environment can be guaranteed.

While libraries should not publish such a file (since libraries are expected to work with a variety of versions rather than only one version of a dependency), projects should always include their lock file when sharing it with other people such that the same behavior can be guaranteed.

## Example

```toml
lock-version = 1
index = "https://git.supercollider.online/magnet-index"
index-http = "https://static.magnet.example/index"

[dependencies]

[dependencies.foo]
version = "0.1.0"
kinds = ["quark"]
source = "index"
checksums = { source-any-any = "987", quark-any-any = "abc" }

[dependencies.bar]
version = "0.0.0"
kinds = ["quark"]
source = "git+https://git.com/favUser/someLegacyQuark.git#4b825dc642cb6eb9a060e54bf8d69288fbee4904"

[dependencies.baz]
version = "0.3.0"
kinds = ["quark", "lepton"]
source = "../baz"

[dependencies.zoo]
version = "1.4.2"
kinds = ["lepton"]
source = "index"
checksums = { lepton-api3_0-linux_x86_64 = "abc", lepton-api3_0-macos_arm64 = "def", source-any-any = "987" }
```

The kinds of artifacts are stored in the lock file such that magnet knows which artifacts of each dependency it needs without having to consult the index, also if one is missing for the machine.

Git dependencies and legacy quarks store their commit SHA in `source`.
Legacy quarks have no semantic version, so their version is `0.0.0`.

`index-http` is copied from the `http` field of the index's `config.json` (if present), such that clients without git (e.g. a browser) can install from the lock file (see [`magnet-index`](index-format.md)).

## Checksums

`checksums` holds the sha256 of every artifact of that version known at lock time, keyed by `{kind}-{abi}-{platform}`.
This way the lock file can be shared and used across platforms.
Only packages from the index have checksums: local dependencies can't be pinned and git dependencies are identified by their commit SHA.
Binaries which get published after the lock file was written are added to it during the installation (see [Missing binary](../guide/leptons.md#missing-binary)).
