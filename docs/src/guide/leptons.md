# Leptons: server plugins

## Server plugins

scsynth and supernova load their plugins from their own `plugins` folder, the system and user Extensions folders and the folders of the `SC_PLUGIN_PATH` environment variable.
If a list of plugin paths is given (`-U`, which sclang passes from `ServerOptions.ugenPluginsPath`), only these paths are loaded and all of the default paths are skipped, including the plugins of SuperCollider itself.
`SC_PLUGIN_PATH` can only add paths, so it can not isolate a project from globally installed plugins.

Like for sclang (see [`sclang_conf.yaml`](projects.md#sclang_confyaml)), magnet therefore lists all paths explicitly: the `plugins` folder of SuperCollider itself, the plugins of each dependency, and the plugins of a lepton which is developed within the project (see below).
Since scsynth also loads the plugins of all sub folders, only the folder `plugins/<platform>/<abi>` of each dependency which matches the server gets listed, i.e. of its platform and with the same major and the highest minor version of the plugin API which is not newer than the one of the server.
These paths are set as `ServerOptions.ugenPluginsPath` of every server by a subclass of `ServerOptions` which magnet generates into `magnet_packages`: it becomes the default options class of `Server` and sets the paths on the servers which already exist.
It gets compiled via the include paths of `sclang_conf.yaml`, so it also applies when the server gets booted from the IDE.
Options which user code creates via `ServerOptions.new` keep the default plugin paths of SuperCollider.
The class determines the paths when sclang compiles it, i.e. the `plugins` folder of the running SuperCollider (via `Platform.resourceDir`) and `magnet_packages` relative to its own file, so it does not depend on a found sclang and always matches the SuperCollider it runs in.
It does not replace plugin paths which the user has set explicitly.

When the project itself contains a lepton, magnet copies the binaries of its `[artifact.lepton]` for the current platform into `magnet_packages/.dev/<name>/plugins` during `magnet install` and `magnet run`, so a local build gets loaded by the server.
Its classes do not need to be copied since the paths of `[artifact.quark]` of the project are already part of the include paths.

## Lepton artifacts

A version which contains a lepton has one lepton artifact per plugin API version and platform, each containing its binaries for scsynth and, if available, supernova within `plugins/<platform>/<abi>/`, while its classes are part of the quark artifact.
Once installed into `magnet_packages/<name>`, scsynth/supernova load the binaries of the folder which matches them (see [Server plugins](#server-plugins)).

A version may lack binaries for some platforms, e.g. because nobody built them yet.
These can be added to the published version later on (see [Publishing a package](publishing.md#publishing-a-package)), and a missing binary is handled during the installation (see [Missing binary](#missing-binary)).

## ABI tag

For binary artifacts, the abi tag only describes the binary compatibility, i.e. the plugin API version of scsynth/supernova for leptons and the plugin ABI of sclang for gluons.
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

## Platform tag

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

## Missing binary

Dependencies are resolved by their versions only, so the lock file does not depend on the platform.
When choosing a version of a lepton, magnet prefers the newest allowed version with a binary for the machine, unless a version is already locked, so the lock file fits the machine which wrote it.
Only the installation picks the artifact which matches the platform and abi tag of the machine.
If the locked version of a lepton has no such artifact, the installation stops and names the newest version of the package which provides one, via `magnet update foo` if the manifest allows this version and otherwise via `magnet add foo@=0.3.1`, or suggests an override if there is none.

An override installs a package from a local folder instead of the index, e.g. a plugin which was built locally or downloaded for a platform which the index does not provide yet:

```toml
[overrides]
foo = { path = "../foo" }
```

If the folder contains a `magnet.toml`, e.g. the project of the lepton itself, magnet installs what `magnet package` would select for the platform, i.e. the files of its `[artifact.quark]` and its binaries of `[artifact.lepton]`.
Otherwise, the folder is copied as it is, e.g. an unzipped release of a plugin.
An override only changes what gets installed: the dependency is still resolved via the index, and the lock file keeps its entry and checksums, so it can still be shared.
The files of an override are not checked against a checksum since they change with every build, and running `magnet install` again copies the current files.

Packages which refer to packages via local paths can not be published via the index!

Binaries can be added to a published version later on, so a lock file may lack the checksum for the platform of the machine.
In this case magnet takes the checksum of the same version from the index, verifies the download against it and adds it to the lock file.
Existing checksums are never changed, and a checksum of the lock file which differs from the index is an error.
`magnet install --locked`, e.g. for a CI, never writes the lock file and fails instead.
