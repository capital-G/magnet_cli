# Magnet design specification

This documents lays out the specification and design of a package manager for the SuperCollider environment named *Magnet*.
The goal is to define a re-producible SuperCollider environment through a file such that projects can be easily and reliably shared and stored.
This will also replace the current Quarks ecosystem (referred to as *legacy-Quarks* within this document) and will extend it by also covering the binary extensions for the server (which will be named *Leptons* within the magnet environment) and the future (SC 3.16?) language extensions called *Gluons*.

The magnet environment consists of

* A `magnet.toml` as a project manifest to make the context to run a project explicit
* A CLI client `magnet-cli` which is used to set up a project, manage its dependencies and takes care of bundling operations for publishing the project on the package index. By publishing a project on the index it becomes a package which can be included by other projects and packages as dependency using semantic versioning.
* A server `magnet-server` which takes care of authentication and indexing all available packages and serving the contents of the packages.
* A git repository `magnet-index` which acts as a distributed database for the package index. This will be updated by the server and used by `magnet-cli` to resolve the dependency tree and request packages from the server.

The need for a package manager seems to be particularly necessary since SuperCollider 3.15 will break the server plugin API, such that server extensions will need to be re-compiled.
Ideally, while re-compiling, people can publish their plugins into the Magnet ecosystem.

## Why not extend the existing Quarks system?

Currently, SuperCollider offers a way to distribute additional Classes via [*Quarks*](https://docs.supercollider.online/Guides/UsingQuarks.html), where the package format and its manager share the same name.
Quarks suffer from the following problems, which all of them are resolved in magnet:

* Quarks only cover sclang classes and do not manage binaries (i.e. server extensions and future sclang plugins)
* Dependencies are not resolved properly, i.e. their version range is not respected. This is a design flaw of Quarks since there is no global index which tracks which versions are available at which location.
* The manifest file `.quark` is written as a sclang dictionary, so every time one needs to reliably access information it is necessary to spin up a sclang interpreter which can be non-trivial in some environments. At the same time its content of the manifest is never verified, resulting in possible broken behavior and allowing for malicious activity.
* Quarks depends on `git`, but `git` is not provided by SuperCollider. This creates an additional barrier for new users.

This makes the legacy Quarks system not as useful, easy and reliable as it should be for archiving, sharing and developing SuperCollider projects.

## Backwards compatibility / Legacy Quarks

Backwards compatibility is valued highly in SuperCollider, so a new package manager should not diverge from this credo.
Therefore, it is possible to depend on existing legacy Quarks (so projects which have a `*.quark` file in a git repository but do not have a `magnet.toml` and are therefore also not present in the `magnet` index) by refering to them either via their name or the location of their git repository, which mimicks the legacy Quarks behavior.
The dependencies of a legacy quark will be tried to be extracted from its `.quark` file through a custom parser and if the dependency exists as magnet package, it will be prefered over installing the dependency via the legacy quark system since magnet allows to resolve version conflicts which the legacy quark system is not capable of.

All names of the legacy Quarks that are tracked in the [current index](https://github.com/supercollider-quarks/quarks/blob/master/directory.txt) are blocked in the new index such that [name-squatting](https://peps.python.org/pep-0541/) on existing projects can not be performed.
This is also the reason why magnet index is relying on a [flat namespace](https://samsieber.tech/posts/2020/09/registry-structure-influence/).
Only authors and maintainers of those existing packages can take the name in the new magnet index, though this can be also decided on a case-by-case basis if the project is abandoned.

Since the versioning of Quarks is not really existing, Quarks will be pinned by their commit hash in order to provide reproducible environments.
Transitive dependencies of quarks will also be added as restrictions to the magnet dependency graph and will be resolved to magnet dependencies if they exist and otherwise will fall back to the most recent version available via `directory.txt`.

In order to provide a smooth transition, published magnet packages can depend on those legacy quarks, though only via a pinned commit hash or tag, not via a branch.

## Magnet project

In the legacy Quark system all Quarks are globally installed to a local [`Quarks.folder`](https://docs.supercollider.online/Classes/Quarks.html#*folder) and will be implicitly added to the currently active [LanguageConfig](https://docs.supercollider.online/Classes/LanguageConfig.html).
While this is an easy way to install legacy Quarks, it also tends to clutter the global environment which will eventually lead to conflicting Quarks due namespace collision through classes or methods.

This setup is practical to provide a default setup to sketch out things, but at the point the sketch needs to be reproducible, reliable and shared it can create all sorts of problems.
In general: The more dependencies, the more possible conflicts, either now or in the future - so it is best practice to pursuit a minimal environment for each project.

Many scripting languages (mainly Python though) suffer from a similar problem: A project relies on some dependencies, but accumulating dependencies in a global environment results in conflicts - at least if project A depends on v1 of X, but project B depends on v2 of X.

The common solution for this is to set up an "isolated environment" for each project (also often called virtual environment), which encapsulates all necessary dependencies for the project.
Magnet will do exactly this - instead of a global `Quarks.folder`, every project will have a sub-folder where all necessary dependencies are stored and loaded when a project is opened.

### Project folders

When creating a new magnet project the following folders will be created

* `classes/` - SC classes of the project
* `dist/` - stores created packages of the project, `.gitignore` this
* `HelpSource/` - schelp documentation of the project, since SCDoc only finds help files in folders with this name
* `magnet_packages/` - storing all dependencies - managed by magnet, `.gitignore` this
    * `magnet_packages/quarks`
    * `magnet_packages/leptons` - which will include scsynth and supernova versions, depending on if both are available via the index
    * `magnet_packages/gluons`
* `tests/` - code related to testing
* `sclang_conf.yaml` - a language config file to be picked up by the IDE, `.gitignore` this since it will be generated by magnet. This may disappear in future magnet versions in case the language config file will be replaced.

Each dependency is placed within the folder of its kind, e.g. `magnet_packages/leptons/foo`, such that it is visible on a folder level which dependencies bring binaries and scsynth only needs to look into `magnet_packages/leptons` for plugins.
Legacy quarks and git dependencies are placed into `magnet_packages/quarks`.
The folder is named after magnet such that it does not collide with folders of other tools, e.g. an `external` folder which holds the C++ dependencies of a lepton.

Since sclang can only have one class declaration for a name, only one version of each package can be installed, so dependencies are not nested or versioned like in npm.

Having this standardized approach allows easier packaging of the source code for releases, yet the exact location of the files can still be changed within the manifest file.

### Contents of package folders

Each folder `magnet_packages/<kind>s/<dependency-name>` mirrors the files which have been selected by the `global-paths` of the package, relative to the root of its project.
This allows legacy quarks to keep their existing folder structure, such that e.g. relative paths to resources or the structure of `HelpSource` keep working.

Leptons additionally contain a `plugins` folder with the scsynth and, if available, supernova binaries.

### `sclang_conf.yaml`

> This is open to change in future version of magnet.

After `magnet install`, `add`, `remove`, `update` or `run`, it will create (or update) a `sclang_conf.yaml` in the root folder of the project.
This can be used by the IDE to pick up all dependencies and code of the magnet project, e.g. via `sclang -l sclang_conf.yaml`.
Note that this sync goes only one-way, so any user changes to `sclang_conf.yaml` would be overwritten when the state of the magnet project changes.

The content of the file is

```yaml
excludeDefaultPaths: true

includePaths:
  # the class library of the found sclang
  - "/Applications/SuperCollider.app/Contents/Resources/SCClassLibrary"
  # base directories of the global-paths of the project, i.e. "classes/" by default
  - "/path/to/project/classes"
  - "/path/to/project/magnet_packages"
  # local dependencies are added via the base directories of their global-paths
```

The base directory of a global path is the part before the first wildcard, e.g. `Classes/**.sc` results in `Classes/`.
All paths are absolute, since sclang resolves relative paths against its working directory, which is not the project root when sclang gets started by the IDE.

`excludeDefaultPaths` keeps globally installed extensions out of the project, but it also excludes the class library of SuperCollider itself, so magnet adds the `SCClassLibrary` of the sclang it finds (via `MAGNET_SCLANG`, the `PATH` or the default installation) as first entry.
If magnet can not find sclang, it writes `excludeDefaultPaths: false` instead and warns that globally installed extensions are compiled as well.

### Server plugins

scsynth and supernova load their plugins from their own `plugins` folder, the system and user Extensions folders and the folders of the `SC_PLUGIN_PATH` environment variable.
If a list of plugin paths is given (`-U`, which sclang passes from `ServerOptions.ugenPluginsPath`), only these paths are loaded and all of the default paths are skipped, including the plugins of SuperCollider itself.
`SC_PLUGIN_PATH` can only add paths, so it can not isolate a project from globally installed plugins.

Like for sclang, magnet therefore lists all paths explicitly: the `plugins` folder of SuperCollider itself, `magnet_packages/leptons`, and the plugins of a lepton which is developed within the project (see below).
These paths are set as `ServerOptions.ugenPluginsPath` of every server by a subclass of `ServerOptions` which magnet generates into `magnet_packages`: it becomes the default options class of `Server` and sets the paths on the servers which already exist.
It gets compiled via the include paths of `sclang_conf.yaml`, so it also applies when the server gets booted from the IDE.
Options which user code creates via `ServerOptions.new` keep the default plugin paths of SuperCollider.
The class determines the paths when sclang compiles it, i.e. the `plugins` folder of the running SuperCollider (via `Platform.resourceDir`) and `magnet_packages` relative to its own file, so it does not depend on a found sclang and always matches the SuperCollider it runs in.
It does not replace plugin paths which the user has set explicitly.

When the project itself is a lepton, magnet copies the binaries of its `platform-paths` for the current platform into `magnet_packages/.dev/<name>/plugins` during `magnet install` and `magnet run`, so a local build gets loaded by the server.
Its classes do not need to be copied since the `global-paths` of the project are already part of the include paths.

## Project manifest `magnet.toml`

The project manifest is a file in the root directory of the project which describes the necessary context for the project.
Although SuperCollider has currently only support for `.json` and `.yaml`, the `.yaml` file format was not chosen for this because it is hard to modify a `yaml` file without re-writing it, i.e. preserving comments.
Almost all modern CLI tools favor `toml` over `yaml`, therefore magnet also follows this.

The file tracks all dependencies and also stores metadata of the project such that it can be bundled and released as a package on the index.
This manifest file does not need to be written manually but is intended to be created and edited through the `magnet` cli, though it is also fine to edit it via hand and a schema exists to assist editing via an editor.

Currently only class extensions have a `.quark` file within the legacy Quark system, though in magnet also Leptons (server plugins) and Gluons (sclang plugins) have such a `magnet.toml` file since they also should be published via the index, making it necessary to provide such a file.

A current sketch of the JSON schema can be accessed via https://gist.github.com/capital-G/bcd3d411b3e4c15597001fbe5f9b371c#file-magnet-spec-json and edited via e.g. https://json.ophir.dev/.
The exact layout of this file is still WIP and any hints regarding pitfalls is highly appreciated.


### Example

```toml
magnet-version = 1

[package]
name = "magnet-test"
version = "0.1.0"
# can either be quark or lepton (a server plugin)
kind = "quark"
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
# we can also reference packages which are not part of the index via git
outsidOfIndex = { git = { url = "http://my.git", branch = "main" } }
# we can also refer to other local magnet projects
# can not publish to index if local package present!
myLocalDependency = { path = "../mySecretProject" }

[artifact]
global-paths = ["Classes/**.sc", "examples/*"]

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

Entrypoints will be an alternative for a standalone version of SuperCollider: Ideally, People would only need the source code and a copy of magnet and everything would be setup accordingly.

One can only run the entrypoints of the root project.

The `kind` of an entrypoint defaults to `sclang`, whose `command` is the path of a script relative to the project root, which gets started via `sclang -l sclang_conf.yaml`.
A `system` entrypoint runs its `command` via the shell.
Both run in the project root, and additional arguments are passed on, e.g. `magnet run test a b`.

### Semantic versioning

Packages need to define their versions using [semantic versioning](https://semver.org/) which defines for a clear ordering and also communicate compatibility issues.

Since magnet can only provide a reliable infrastructure, it depends on its users to actually build a reliable package environment!
Therefore, some advice:

* Always add, never remove any exposed API! I.e. do not change or remove class names, methods, argument names or positions. `pr` scoped methods act as the only exemption.
* Prefer keyword arguments over positional arguments to be more resilient towards API changes of other packages. This introduces a performance overhead, but this is probably tolerable for having more resilient code.
* Treat bumping a major version like renaming the package since it is unclear if previous code which relied on the package still works. Use versions `<1.0` to indicate that this package has still an unstable API.

At some point it will be hopefully possible to indicate a version as `alpha`, `beta` or `rc` in order to publish a version for testing which will be ignored when resolving dependency unless explicitly requested.

#### Version range restriction

Version restrictions follow the ones from [`cargo`](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html) and [`npm`](https://docs.npmjs.com/cli/v12/configuring-npm/package-json#dependencies)

Written | Comment
--- | ---
`1.2.3` | Equivalent to `^1.2.3`
`^1.2.3` | `>=1.2.3`, `<2.0.0`
` ` (empty) | Equivalent to `*`
`*` | `>=0.0.0`
`~1.2.3` | `>=1.2.3`, `<1.3.0`

Pre-releases are not supported yet.

#### SuperCollider version

The `sclang` entry of `[package]` states which versions of SuperCollider a package works with, using the version ranges from above, e.g. `sclang = ">=3.13"`.
This applies to every kind of package, since leptons and gluons also ship `.sc` classes.

While resolving, a version of a dependency is only chosen if its `sclang` range allows the lower bound of the project's `sclang` range, so the lock file holds for the whole range of the project, like it holds across platforms.
Older versions of a dependency are tried if a newer one requires a more recent SuperCollider.
Like [uv](https://docs.astral.sh/uv/) does for `requires-python`, upper bounds of dependencies are ignored, since defensive caps would block the resolution, so packages should only declare a lower bound.
An upper bound only counts on the root project.

If the project declares no `sclang` range, the dependencies are resolved without it and magnet warns if a locked package requires a newer version than the sclang found on the system.
`magnet init` writes `>=<major>.<minor>` of the found sclang.

The SuperCollider version belongs to a package version and not to its artifacts, so it is not part of the filename of an artifact (see Packaging): the resolver picks versions and the artifacts are only picked afterwards, so a tag would fail at install instead of falling back to an older compatible version.

### Lock file`magnet.lock`

Magnet will also create a lock file which stores the exact version of every installed dependency.
This is particularly relevant for transient dependencies, and only with a lock file a full replication of the environment can be guaranteed.

While libraries should not publish such a file (since libraries are expected to work with a variety of versions rather than only one version of a dependency), projects should always include their lock file when sharing it with other people such that the same behavior can be guaranteed.

#### Example

```toml
lock-version = 1
index = "https://git.supercollider.online/magnet-index"
index-http = "https://static.magnet.example/index"

[dependencies]

[dependencies.foo]
version = "0.1.0"
kind = "quark"
source = "index"
checksums = { any-any = "abc" }

[dependencies.bar]
version = "0.2.0"
kind = "quark"
source = "git+https://git.com/favUser/someLegacyQuark.git#4b825dc642cb6eb9a060e54bf8d69288fbee4904"

[dependencies.baz]
version = "0.3.0"
kind = "lepton"
source = "../baz"

[dependencies.zoo]
version = "1.4.2"
kind = "lepton"
source = "index"
checksums = { api3_0-linux_x86_64 = "abc", api3_0-macos_arm64 = "def", source-any = "987" }
```

The kind is stored in the lock file such that magnet knows the folder of each dependency within `magnet_packages` without having to consult the index.

`index-http` is copied from the `http` field of the index's `config.json` (if present), such that clients without git (e.g. a browser) can install from the lock file (see `magnet-index`).

##### Checksums

`checksums` holds the sha256 of every artifact of that version known at lock time, keyed by `{abi}-{patform}`.
This way the lock file can be shared and used across platforms.
Only packages from the index have checksums: local dependencies can't be pinned and git dependencies are identified by their commit SHA.
Binaries which get published after the lock file was written are added to it during the installation (see Missing binary).

## `magnet` as local CLI

A `magnet` CLI will the primary way for people to interact with the package manager.
It will be a binary which need to be downloaded and installed by the user and will currently not be part of the SuperCollider distribution.

The magnet client mainly is used to

* create a new magnet project
* add dependencies to a magnet project and resolve its dependencies recursively
* set up the environment for an existing magnet project
* bundle packages for release
* publishing packages on the index

`magnet add`, `remove` and `update` change the manifest and the lock file and install the resulting dependencies into `magnet_packages/` right away, which can be skipped via `--no-install` such that only the files get written.
`magnet install` sets up an existing project, e.g. after cloning it, by installing the dependencies of its lock file.

### Packaging

Once a project should be published it needs to be packaged and sent to the server for indexing.

```bash
magnet package
```

Since magnet knows via the package manifest which files make up the package, it will be put into a `.tar.gz` file and exported into the `dist` folder.

The files of a package are selected by the globs of the `[artifact]` table of the manifest, relative to the project root, and not by a `.gitignore` file:

* `global-paths` - files of every artifact, e.g. the `.sc` classes and `HelpSource`
* `platform-paths` - the binaries of a lepton or gluon, keyed by platform tag, e.g. `platform-paths = { macos_arm64 = ["build/*.{scx,dylib}"], linux_x86_64 = ["build/*.{scx,so}"] }`, which also selects the shared libraries a plugin needs next to it
* `source-paths` - the files of the source package of a lepton or gluon, e.g. the C++ sources and the `CMakeLists.txt`, required for these kinds
* `exclude` - files which are never packaged, even if a glob above selects them

Globs work like in `.gitignore`: `*` matches within a folder and `**` across folders, e.g. `Classes/**.sc` selects all `.sc` files within `Classes`.
Hidden files and folders (e.g. `.git`), as well as `dist`, `magnet_packages` and `sclang_conf.yaml` of the project root are never packaged.
A package keeps the relative layout of the selected files, so it mirrors the project (see Contents of package folders), since legacy quarks load resources relative to their class files and SCDoc relies on the folder structure of `HelpSource`.
Only the binaries of `platform-paths` are moved into `plugins/`.
A file which is selected by multiple globs of `global-paths` is only packaged once, but packaging fails if a file is selected by `global-paths` and `platform-paths`, or if a glob does not select any file, since this is probably a typo and a published artifact can never be replaced.

Every artifact also contains the `magnet.toml` of the project at its root, such that the server reads the manifest from the uploaded artifact (see Publishing a package), and the readme file of `[metadata]` (`README.md` by default, if it exists) at its path.
A quark has no dedicated source package, since its artifact already is the source.

The naming convention for the `.tar.gz` file follows

```
{name}-{version}-{kind}-{abi}-{platform}.tar.gz
```

where `name` is the normalized package name, `kind` the kind of the package (`quark`, `lepton` or `gluon`) and `abi` and `platform` the tags of the artifact.
These filenames are only ever built from the fields of the index (see `config.json` of `magnet-index`) and never parsed, so a `-` within a name is not a problem.
Tags use `_` instead of `-` and only consist of `a-z`, `0-9` and `_`, so they are URL-safe.

For platform-dependent binary packages, so Leptons and Gluons, magnet will detect the platform and the ABI of the found SuperCollider, but it is possible to specify both explicitly through CLI flags and environment variables, e.g. for cross-compilation.
Leptons and Gluons will also upload a package of the source code to the index, which consists of the files selected by `source-paths`.

```
{name}-{version}-{kind}-source-any.tar.gz
```

Since Quarks are not bound to a platform or a binary interface, they use `any` for both tags, e.g. `foo-bar-1.2.0-quark-any-any.tar.gz`.
The supported SuperCollider versions are not a tag but the `sclang` range of the manifest (see SuperCollider version), like Python wheels keep `Requires-Python` in their metadata.

For binary packages, the abi tag only describes the binary compatibility, i.e. the plugin API version of scsynth/supernova for leptons and the plugin ABI of sclang for gluons.
The abi tag of a lepton follows what scsynth checks when it loads a plugin, which is the version of the plugin API, so the tag is `api<major>_<minor>`, e.g. `api4_1`.
Starting with SuperCollider 3.15 the plugin API uses semantic versioning: the major version increases with every breaking change of the plugin interface, the minor version with additions which do not break existing plugins, e.g. new functions at the end of the interface table.
Earlier versions only have a single number, which counts as the major version, e.g. `api3_0`.
This is not a fixed list: both numbers can be any integer.
scsynth loads a plugin with the same major version and a minor version up to its own, so the minor version of a lepton artifact is the one the plugin targets, and a version of a lepton may have several artifacts for a platform, e.g. `api4_0` and `api4_2`.
The installation picks the one with the highest minor version which the server loads.
Should SuperCollider ever ship incompatible variants of the same plugin API side by side, e.g. scsynth with 64 bit samples, scsynth needs to tell their plugins apart and the tag gets a suffix, e.g. `api5_0_f64`.
scsynth and supernova share the same plugin API version, so one artifact contains the binaries for both.

SuperCollider | abi tag
--- | ---
3.6 - 3.8 | `api2_0`
3.9 - 3.14 | `api3_0`
3.15 | not released yet, expected to be `api4_0`

magnet derives the abi tag from the version of the found SuperCollider via this table.
For versions it does not know, e.g. development builds, it needs to be set explicitly, e.g. `api4_1`.

The archive is a gzip compressed tar file in the plain *ustar* format without GNU or pax extensions, such that a small reader is enough to unpack it, e.g. within a browser.
Long paths use the `prefix` field of ustar (155 characters for the directory in addition to the 100 characters of the name), and packaging fails for longer paths.
It only contains regular files and directories, and its entries are sorted and written with a fixed modification time, uid/gid 0 and mode 0644 (0755 for directories and executables), such that packaging the same files always results in the same archive.

The platform tag is one of the following values

* `linux_x86_64`
* `linux_arm64`
* `linux_armv7` - 32 bit ARM, e.g. older versions of Raspberry Pi OS
* `macos_x86_64`
* `macos_arm64`
* `windows_x86_64`
* `windows_arm64`
* `wasm32_emscripten` - reserved for SuperCollider within the browser, which may be able to load leptons via Emscripten side modules in the future

The architectures are named the same on every operating system.
Binaries for Linux need to run on glibc 2.35 (Ubuntu 22.04) and newer, so they should be built on such a system.
There is no universal tag for macOS: a universal binary gets packaged once for each architecture.

Before uploading the asset to the index a user is expected to inspect its content since the package index only allows to add, never to delete.

#### Package kinds

A package can be one of 2 kinds, *quark* or *lepton* (aka server plugins).
In the future it would be possible to extend this e.g. with *gluon*, which would be a binary plugin for sclang.

The kind of package does not take part in the dependency resolution, there can only be one kind of package for a given name.
In case a future *gluon* package also requires a *lepton* package, these should be two separate magnet projects which depend on each other.

#### Lepton artifacts

A version of a lepton has one artifact per plugin API version and platform, each containing the files of its `global-paths` (e.g. the `.sc` classes and help files) as well as its binaries for scsynth and, if available, supernova within `plugins/`.
Once installed into `magnet_packages/leptons/<name>`, sclang picks up its classes like the ones of a quark, and scsynth/supernova load the binaries of its `plugins/` folder.

A version may lack binaries for some platforms, e.g. because nobody built them yet.
These can be added to the published version later on (see Publishing a package), and a missing binary is handled during the installation (see Missing binary).

#### Valid package names

Package names are

* case-insensitive
* `_`/`-` insensitive

#### Missing binary

Dependencies are resolved by their versions only, so the lock file does not depend on the platform.
When choosing a version of a lepton, magnet prefers the newest allowed version with a binary for the machine, unless a version is already locked, so the lock file fits the machine which wrote it.
Only the installation picks the artifact which matches the platform and abi tag of the machine.
If the locked version of a lepton has no such artifact, the installation stops and names the newest version of the package which provides one, via `magnet update foo` if the manifest allows this version and otherwise via `magnet add foo@=0.3.1`, or suggests an override if there is none.

An override installs a package from a local folder instead of the index, e.g. a plugin which was built locally or downloaded for a platform which the index does not provide yet:

```toml
[overrides]
foo = { path = "../foo" }
```

If the folder contains a `magnet.toml`, e.g. the project of the lepton itself, magnet installs what `magnet package` would select for the platform, i.e. the files of its `global-paths` and its `platform-paths` binaries within `plugins/`.
Otherwise, the folder is copied as it is, e.g. an unzipped release of a plugin.
An override only changes what gets installed: the dependency is still resolved via the index, and the lock file keeps its entry and checksums, so it can still be shared.
The files of an override are not checked against a checksum since they change with every build, and running `magnet install` again copies the current files.

Packages which refer to packages via local paths can not be published via the index!

Binaries can be added to a published version later on, so a lock file may lack the checksum for the platform of the machine.
In this case magnet takes the checksum of the same version from the index, verifies the download against it and adds it to the lock file.
Existing checksums are never changed, and a checksum of the lock file which differs from the index is an error.
`magnet install --locked`, e.g. for a CI, never writes the lock file and fails instead.

### Publishing a package

After the magnet package has successfully been created, it can be uploaded to the package index via the client or the website.

```bash
magnet publish [FILES]...
# e.g.
magnet publish dist/mypackage-0.1.0-quark-any-any.tar.gz
```

Without a file, all artifacts of the current version within `dist` are published.

Publishing a version consists of three steps, such that the index never contains a version which was only uploaded halfway:

1. The client creates a *draft* of the version, which also registers the package if it does not exist yet.
2. The client uploads the artifacts to the draft, each with its sha256 checksum. For a lepton or gluon, the client also uploads the source package.
3. The client *finalises* the version, which is only possible once a quark has its `any-any` artifact and a lepton or gluon its `source-any` artifact.

Only finalised versions are written into the index.
A draft which never gets finalised (e.g. because a CI run failed) is only visible to the owners of the package and stays until they delete it, after which its version can be drafted again.
All steps can be repeated, such that e.g. each job of a CI matrix can publish its own binaries: an existing draft or finalised version is re-used, and uploading an identical artifact again is not an error.

Artifacts can be added to a finalised version later on, e.g. binaries for an additional platform, but they can never be replaced.
The first published version fixes the kind of the package, and versions of a different kind are rejected (an admin can override this).

Publishing is refused if the manifest contains `path` dependencies, `[overrides]`, packages from a different index, or git or legacy quark dependencies which are not pinned via a tag or ref, since others could not reproduce them.
The client checks this before uploading, and the server checks it again.

#### Checks of the server

The client packages the artifacts, but the server does not trust anything the client claims:

* The server computes the sha256 checksum of an artifact itself, and the index uses this value. The checksum sent by the client only guards the transfer and needs to match.
* The manifest is read from the `magnet.toml` within the artifact (see Packaging), not from a separately sent copy, so the index and the contents of an artifact can not disagree. All artifacts of a version need to agree on the fields which are written into the index, i.e. name, version, kind, `sclang` and dependencies, and dependencies on magnet packages need to exist in the index.
* The archive is inspected but never extracted: the server streams through the tar headers without writing any files, and checks the archive format (see Packaging), that all paths are relative and unique (also when ignoring the case), and that they are selected by the globs of the manifest.
* Against gzip bombs, the size of an upload is limited, and while streaming, the unpacked size, the number of entries and the length of the paths are counted, such that the inspection stops as soon as a limit is reached.

These checks only make sure that a package is well-formed, its contents, e.g. code within `*initClass` or the binaries, are not checked.

### Why not sclang

While writing a package manager client solely in `sclang`, this faces multiple problems

* By not relying on sclang as a dependency it is possible to manage installations of sclang via magnet. This allows to only install magnet, which then will set up SuperCollider.
* We probably want to publish packages via a CI - and making sclang work in a CI environment is somehow hard
* The networking stack of sclang is not as advanced as it could be
* It is possible to "brick" sclang with an installation of an extension (e.g. name collision of classes). Having an outside tool allows to resolve this situation without manual intervention by "rolling back".
* Resolving dependencies is an NP-hard problem and requires a rather complex algorithm to make this still somehow efficient. Other programming languages offer implementation of such an algorithm, so it is not necessary to re-invent the wheel here.
* The CLI capabilities of sclang are somehow limited - e.g. no autocomplete etc.
* We need `git` (for local dependency resolution) and `tar` (for bundling the files) which are currently not part of SuperCollider

There was also some discussion if the client should be written in Python to make development more accessible, but installing a Python application is rather complex and does not offer the same longevity as a binary, so it was decided to go for a binary application.
Rust was chosen because the client requires internet access, OAuth2 authentication, a dependency resolver algorithm and a good CLI - all of which are somehow hard to get right from the start in C++, so it was decided to go with a language which avoids memory related bugs.

### git integration

Although magnet includes a git client and its project structure is suited to be managed via git, magnet tries to follow the Unix approach and will therefore not cover any `git` management of the project.
People are encouraged though to use a git repository and tag the commit which they used to upload to the index.

## `magnet-server`

The magnet server keeps track of all available packages and manages authentication of users when they publish a new version of a package through the CLI.
It also stores all the assets of each package and servers them to a client during installation of dependencies.

The server is written in Django and provides a basic frontend and a REST API to interact with it.
Ideally there will be two available instances of this server (and index): One for production and one for testing.

### Storage

Since it is unclear how big the storage of such a project would become over time I decided to directly go with an object storage solution rather than having a local-only approach which later need to be adapted to a distributed one.

Since MinIO has become unusable, I settled for [Garage](https://garagehq.deuxfleurs.fr/) to have a local S3-environment, but this may be later replaced by a S3-like service.

### Authentication

Social logins will likely not be made available.
OAuth2 will be used to authenticate CLI clients.
`magnet login` opens the authorization flow (with PKCE, since the CLI can not keep a client secret), stores the token in the magnet home folder for each server and refreshes it when it expires, and `magnet logout` revokes it.
It will be possible to generate longer lasting tokens for a CI environment on the website, which are limited to publishing and will be passed via the `MAGNET_TOKEN` environment variable.
Such a token is sent like any other OAuth2 token and is never written to disk by the client.

The website of the index itself also allows to administrate packages, but since a simple frontend is used we can get away with using a session/cookie based approach.

## `magnet-index`

Since dependency resolving is a NP hard problem it may be necessary to go through all available versions of a package, which may also have changing dependencies over its version history.
Although the server has this information available, it would be really slow and put a lot of strain on the server to request this information for every package and installation.

Instead, the server will insert some metadata for every accepted version and asset of package into a dedicated file for each package.
This file is part of a git repository, called `magnet-index`, which acts as a distributed database.
Only the `magnet-server` can write into this repository.

The magnet CLI will clone this repository which contains the list of available packages and update it occasionally.
Since this repository stores all available packages, versions and assets, is can be used to resolve the requested dependencies locally, which improves performance and reduces load on the server dramatically.

This index will be split, i.e. instead of a flat namespace with all its files, we will use an approach which organizes the packages by its first letter into a sub-folder, i.e. `foobar` would be placed in `f/foobar.jsonl`.
This uses the normalized name of the package as a basis, i.e. the normalized representation of `DDW_Common` is `ddw-common` within magnet.

Each file while then have a one-lined JSON for each release, which covers

```json
{"name": "foo", "version": "0.1.3", "kind": "quark", "sclang": ">=3.13", "dependencies": {"bar":  "<0.2.0"}, "yanked": false, "artifacts": [{"abi":  "any", "platform": "any", "sha256":  "abc"}]}
```

`sclang` is the range of the package's manifest and is omitted if the manifest has none.

The kind of a package is fixed by its first published version and can not be changed afterwards, so all lines of a package share the same kind.
If e.g. a quark later also wants to ship a server plugin, the plugin needs to be published as a dedicated lepton package on which the quark depends.

Legacy quarks can also be used as dependencies by providing

```json
{"baz": {"legacy-quark": true, "git": {"ref": "<git sha>", "url":  "<link-to-repo>"}}}
```

Sources (of binary packages) are represented via

```json
{"abi": "source", "platform": "any"}
```

Such an entry will be inserted by the index service as a single writer on this index and can be regenerated from the DB of the index.

Additionally, each index needs to implement a `config.json` at the root which implements

```json
{"index-format": 1, "dl": "<url template>", "http": "<base url>", "api": "<server url>"}
```

`api` is the URL of the `magnet-server` which belongs to this index, used to log in and publish, such that the URL of the index is the only thing which needs to be configured.
It can be overridden via `MAGNET_API_URL` / `--api-url`, e.g. for a local test server.

such that updates to the index can be made and also the download url can be changed in one position in case the packages need to migrate to a different URL.
The URL template needs to be configured in such a way that each artifacts results in a unique and stable URL: a package has only one kind, a version has at most one artifact per abi and platform, and artifacts are never replaced.
If the template changes, the old URLs are redirected to the new ones.
The following pattern should be used such that people can download artifacts manually:

```
https://static.magnet.example/dl/{name}/{version}/{name}-{version}-{kind}-{abi}-{platform}.tar.gz
```

where `name` is the normalized name, `version` and `kind` are used from the chosen line and `abi` and `platform` from the selected artifact.

A browser can not clone a git repository, so the server also publishes the files of the index as static files (`config.json` and `<letter>/<name>.jsonl`), similar to the sparse index of Cargo.
The optional `http` field points to the base URL of this mirror, and can be overridden via `MAGNET_HTTP_INDEX_URL` / `--http-index-url`, e.g. for a local test server.
The git repository stays the source of truth, and if the mirror moves, its old URLs are redirected like the ones of `dl`.

Beyond that, the server also provides an API to obtain more information (e.g. summary, dl-count, authors, readme, tages) about a given package and its version.
The git index stores the bare minimum for dependency resolution and how to obtain the downloadable artifact, such that the footprint of the local git index on each device is kept to a minimum.

> Cargo is following a similar approach - take a look at their index which is located at https://github.com/rust-lang/crates.io-index

### Yanking

Once an asset of a version has been published it is **not** possible for the user to delete it since this would allow to break existing environments.
The package index is therefore append only!
Instead, it is possible to *yank* a version - a yanked version will be ignored during the resolving of future projects, but if a client requests this specific asset it will still be served (though with a warning and maybe manual intervention).
The owners of a package can yank a version, and undo this, via the website.
Projects which already have a yanked version in their lock file keep it, and `magnet install` warns about it.

### Deletion

Yanking is the normal way to retract a version, but some cases require to actually delete a version or a whole package:

* violations of the CoC
* malware
* legal takedowns
* leaked secrets

This is only an index-admin operation.
A deletion removes the version from the DB of the server, since otherwise regenerating the index from the DB would bring it back, and its artifacts from the storage.
The version number (or the name of a deleted package) can never be used again, unless an admin frees the name, such that a lock file can never refer to different contents under the same version.
Downloads of a deleted artifact answer with `410 Gone`, so lock files which refer to a deleted version fail to install with a clear error and need to be resolved again via `magnet update <name>`.

### Trust

The checks of the server (see Publishing a package) only make sure that a package is well-formed.
Installing a package means trusting its authors: the classes of a quark are executed by sclang (e.g. within `*initClass`), and the binaries of leptons and gluons are arbitrary native code which runs with the permissions of the user.
Signing artifacts and trusted publishing (i.e. publishing only from a CI of the package's repository, without long-lived tokens) are not covered yet.

## Integration

Since there is already an existing solution, there needs to be not just some incentive to use it, but it also needs to be easily accessible to the user by integrating it into the ecosystem.

### sclang

Like `Quarks` there will be a `Magnet` class which communicates with the magnet CLI.
magnet provides it within magnet projects by generating it into `magnet_packages`, like the class for the server plugins, so it gets released together with magnet and always matches the CLI which installed the project, instead of being part of the standard library of SuperCollider.

### scsynth / supernova

Since dependencies are stored locally instead of globally it is necessary to allow loading arbitrary folders as source for plugins within scsynth/supernova.
This is already possible via `ServerOptions.ugenPluginsPath`, though it requires to also list the plugins of SuperCollider itself (see Server plugins).

### sc-ide

Instead of just opening files, there should be a way to also open a project which sets up the IDE accordingly.
Maybe it is possible to extend the already existing Session view for this, since sc-ide always have one implicit interpreter - therefore it should be avoided to be able to switch between different interpreters (global one, magnet-scoped one) in order to avoid confusion.

There is also a file explorer in the works for the IDE (see https://github.com/supercollider/supercollider/pull/6549) which would help navigation within a magnet project.

There should also be a way to open a terminal within the IDE which either opens the current working directory of sclang or the root of a magnet project.

### sc3-plugins

Since sc3-plugins is maintained by the SC developers and has become somehow a default extension for SC, it can act as a great way to test out the magnet packaging system.
For 3.15, sc3-plugins would be additionally distributed via a magnet package.

## Maintenance

While most effort is to create all these tools in the first place, there is also some continuous maintenance effort necessary.

* Since the Django server is exposed to the internet, it is necessary to keep this up-to-date. The server and its assets should also be backed-up regularly!
* The client only needs update in case we want to extend the existing manifest format or change how the API is exposed. Since we link OpenSSL dynamically, it should also obtain the most crucial security and certificate updates.

Additional to this, there is also some cost associated with providing the server to host and serve data:

* The server load itself is probably not too high, so it is probably fine with running on a machine with 8GB and 8 cores. Currently, such a server costs around 10 Euro/month. For the beginning I will host it on my own server which has plenty of resources available.
* The object storage scales differently: I will install Garage on my server and will give it up to 400 GB of storage. The moment this becomes too small we need to figure out if we switch to something S3 compatible - e.g. Hetzner provides a S3 plan which costs for 1TB of storage and 3TB of traffic around 10 Euro/month.

It would also be great if there would be some people doing moderation work to resolve reports of packages etc.

## Future ideas

Things that become possible with magnet but are currently outside the scope of a v 1.0.

### Installing SuperCollider via `magnet` CLI

The `magnet` CLI could also install SuperCollider if no SuperCollider can be found.
This also allows to manage multiple SuperCollider versions for each project.
This would be really interesting for RaspberryPi and Linux (though it is really hard there w/o static builds).

See [pyenv](https://github.com/pyenv/pyenv) or [rustup](https://rust-lang.github.io/rustup/concepts/index.html) for this.

### Hosting documentation

Similar to <https://docs.rs/> we could also build and host the documentation of each package.
This should be a dedicated service, but this only becomes possible if we have a package index through which we know what kind of packages exist in the first place.

### Indexing classes and methods

Namespace collisions are really annoying and magnet has no way to avoid it.
Yet it would be possible to track all available classes and their methods, so during upload.

This is a somehow harder task since it either requires a sclang runtime on the client and server side (which introduces security risks since we are executing arbitrary code on the server, so this would need to be run in a strong isolation) or require a lightweight parser which does not evaluate the code.

### Workspaces and features

Cargo has a great feature called workspaces, see https://doc.rust-lang.org/cargo/reference/workspaces.html.
This allows to have meta-packages which are really great for mono-repos, such as `sc3-plugin`.
Once we have workspaces available, we could split up `sc3-plugins` into their own magnet projects, but `sc3-plugin` as a meta-package would guarantee that they are compatible with each other.

### Ideas regarding backwards compatibility

Currently, there is always much discussion involved if we actually need to change existing behavior within the class library or server plugins (we most of the time only do this when we come across a faulty implementation).

With having a reliable package manager it would be possible to fix the faulty behavior in a new version without breaking existing code b/c the old projects would still reference the old package.
This would be really impactful if we make the standard library a workspace meta-package since this would allow to develop drop-in replacements of existing code w/o breaking old code.

### Build system

Serving binary files is desirable over having to build everything oneself, but in case the binaries are missing for a given platform or ABI it should be possible to build the binaries from the given source via e.g. CMake.
For further information see

* Python:[`pyproject.toml` build-system](https://packaging.python.org/en/latest/guides/writing-pyproject-toml/#declaring-the-build-backend)
* Python: [`setuptools`](https://setuptools.pypa.io/en/latest/)

## Acknowledgements

The following resources were a great help for designing magnet

* [Cargo](https://github.com/rust-lang/cargo) and its [index format](https://doc.rust-lang.org/cargo/reference/registry-index.html#index-format) for Rust
* [uv](https://docs.astral.sh/uv/) for Python
* [pip](https://github.com/pypa/pip), [`pyproject.toml`](https://packaging.python.org/en/latest/guides/writing-pyproject-toml/) and [pypi](https://pypi.org/) for Python
* [npm](https://www.npmjs.com/) and [`package.json`](https://docs.npmjs.com/cli/v10/configuring-npm/package-json) for Node.js
* [Deken](https://deken.puredata.info/) for pd
* [PubGrub: Next-Generation Version Solving](https://nex3.medium.com/pubgrub-2fb6470504f)
* [Speculation Keynote - Rich Hickey](https://www.youtube.com/watch?v=oyLBGkS5ICk) about package and dependency management within Clojure
