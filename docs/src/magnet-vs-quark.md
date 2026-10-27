# Magnet vs. Quark

## Features

Feature | Magnet | legacy-Quark
--- |---| ---
Manages binaries such as server plugins? | Yes | No
Isolated environments? | Automatically | Manually through `sclang_conf.yaml`
Environments are reproducible? | Yes | No, dependency resolution of Quarks is not working correctly
Sharing environments through code? | Yes | Limited to Quarks. Additionally not stable because dependencies are not pinned
How packages are published? | Packages are published via an index which gets mirrored to a git repository | Via `directory.txt` in the [quarks git repo](https://github.com/supercollider-quarks/quarks)
De-centralized? | No, but custom indices are supported | Partially, because it is indefinied tied to GitHub repository [quarks git repo](https://github.com/supercollider-quarks/quarks)
Works without sclang interpreter? | Yes, environment are managed via a toml file | No, inspecting a quark file requires the interpreter
Requires additional programs beyond sclang? | Yes, requires `magnet` binary | Yes, requires `git` to be installed on the system
Is supported in wasm environments? | Yes | No, `git` is not available in browser and GitHub has Cross Origin restrictions, so downloading artifacts within a tab is not possible

## Why not extend the existing Quarks system?

Currently, SuperCollider distributes additional Classes via [*Quarks*](https://docs.supercollider.online/Guides/UsingQuarks.html), where the package format and its manager share the same name.
Quarks suffer from the following problems, which all of them are resolved in magnet:

* Quarks only cover sclang classes and do not manage binaries (i.e. server extensions and future sclang plugins)
* Dependencies are not resolved properly, i.e. their version range is not respected. This is a design flaw of Quarks since there is no global index which tracks which versions are available at which location.
* The manifest file `.quark` is written as a sclang dictionary, so every time one needs to reliably access information it is necessary to spin up a sclang interpreter which can be non-trivial in some environments. At the same time its content of the manifest is never verified, resulting in possible broken behavior and allowing for malicious activity.
* Quarks depends on `git`, but `git` is not provided by SuperCollider. This creates an additional barrier for new users.

This makes the legacy Quarks system not as useful, easy and reliable as it should be for archiving, sharing and developing SuperCollider projects.

Legacy quarks can be used within magnet without problems, though Quark maintainers are invited to port their legacy quarks to the magnet index.
For backwards compatibility, see [Installing legacy quark dependencies](guide/dependencies.md#installing-legacy-quark-dependencies).
