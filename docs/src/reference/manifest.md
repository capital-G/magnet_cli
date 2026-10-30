# Project manifest `magnet.toml`

The project manifest is a file in the root directory of the project which describes the necessary context for the project.
Although SuperCollider has currently only support for `.json` and `.yaml`, the `.yaml` file format was not chosen for this because it is hard to modify a `yaml` file without re-writing it, i.e. preserving comments.
Almost all modern CLI tools favor `toml` over `yaml`, therefore magnet also follows this.

The file tracks all dependencies and also stores metadata of the project such that it can be bundled and released as a package on the index.
This manifest file does not need to be written manually but is intended to be created and edited through the `magnet` cli, though it is also fine to edit it via hand and a schema exists to assist editing via an editor.

Currently only class extensions have a `.quark` file within the legacy Quark system, though in magnet also Leptons (server plugins) and Gluons (sclang plugins) have such a `magnet.toml` file since they also should be published via the index, making it necessary to provide such a file.

A current sketch of the JSON schema can be accessed via <https://gist.github.com/capital-G/bcd3d411b3e4c15597001fbe5f9b371c#file-magnet-spec-json> and edited via e.g. <https://json.ophir.dev/>.
The exact layout of this file is still WIP and any hints regarding pitfalls is highly appreciated.

## Example

```toml
magnet-version = 1

[package]
name = "magnet-test"
version = "0.1.0"
# supported versions of SuperCollider
sclang = ">=3.13"

[[entrypoint]]
name = "hello"
command = "echo hello"
kind = "system"

[[entrypoint]]
name = "test"
command = "test.scd"
kind = "sclang"

[[entrypoint]]
name = "run"
command = "run.scd"
kind = "sclang"

[dependencies]
jitlib-extensions = "0.5.*"
# allows to rely on old quarks
ddwCommon = { legacy-quark = true }
# the dependencies of its .quark file are not used, so they need to be listed here
someQuark = { legacy-quark = true, ignore-quark-deps = true }
# we can also reference packages which are not part of the index via git
outsidOfIndex = { git = { url = "http://my.git", branch = "main" } }
# we can also refer to other local magnet projects
# can not publish to index if local package present!
myLocalDependency = { path = "../mySecretProject" }

[artifact.quark]
paths = ["Classes/**.sc", "examples/*"]

# uncomment this once the project builds server plugins
# [artifact.lepton]
# platform-paths = { macos_arm64 = ["build/*.{scx,dylib}"] }

[metadata]
license = "GPL-3.0"
tags = ["awesome", "jitlib"]
# a single line which is shown in lists and search results
summary = "An awesome extension for JITLib"
description = """
This is a long description
"""
authors = [
  { name = "hello", email = "hello@hello.com" },
  { name = "world", email = "world@world.com" },
]
```

The version ranges of `sclang` and `[dependencies]` are described in [Semantic versioning](../guide/dependencies.md#semantic-versioning), the globs of `[artifact]` in [Packaging](../guide/publishing.md#packaging).

## Entrypoints

Entrypoints will be an alternative for a standalone version of SuperCollider: Ideally, People would only need the source code and a copy of magnet and everything would be setup accordingly.

One can only run the entrypoints of the root project.

The `kind` of an entrypoint defaults to `sclang`, whose `command` is the path of a script relative to the project root, which gets started via `sclang -l sclang_conf.yaml`.
A `system` entrypoint runs its `command` via the shell.
Both run in the project root, and additional arguments are passed on, e.g. `magnet run test a b`.

## Artifact kinds

A version of a package contains one or more kinds of artifacts, *quark* (sclang classes) or *lepton* (aka server plugins), each declared by its table in `[artifact]` (see [Packaging](../guide/publishing.md#packaging)).
In the future it would be possible to extend this e.g. with *gluon*, which would be a binary plugin for sclang.
Each kind has its own artifacts and its own abi tag, such that the binaries of different kinds never need to be combined.

The kinds do not take part in the dependency resolution and can change between versions, e.g. a quark can also contain a lepton from its next version on.
A package can also be split into several packages, e.g. a lepton without classes on which a quark depends, but this is never required.
The source is not a kind, since every version has one.

## Valid package names

Package names are

* case-insensitive
* `_`/`-` insensitive
