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
* `docs/` - schelp documentation of the project
* `external/` - storing all dependencies - managed by magnet, `.gitignore` this
    * `external/quarks`
    * `external/leptons`
    * `external/gluons`
* `tests/` - code related to testing

Having this standardized approach allows easier packaging of the source code for releases, yet the exact location of the files can still be changed within the manifest file.

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
min-sclang-version="3.13.0"

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
description = """
This is a long description
"""
authors = [
  { name = "hello", email = "hello@hello.com" },
  { name = "world", email = "world@world.com" },
]
```

Entrypoints will be an alternative for a standalone version of SuperCollider: Ideally, People would only need the source code and a copy of magnet and everything would be setup accordingly.

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

### Lock file`magnet.lock`

Magnet will also create a lock file which stores the exact version of every installed dependency.
This is particularly relevant for transient dependencies, and only with a lock file a full replication of the environment can be guaranteed.

While libraries should not publish such a file (since libraries are expected to work with a variety of versions rather than only one version of a dependency), projects should always include their lock file when sharing it with other people such that the same behavior can be guaranteed.

#### Example

```toml
lock-version = "1.0.0"
index = "http://git.supercollider.online/magnet-index"

[dependencies]
foo = { version = "0.1.0", source = "index", checksum = "sha256" }
bar = { version = "0.2.0", source = "git+<url>#<sha>", checksum = "sha256" }
baz = { version = "0.3.0", source = "../baz", checksum = "sha256" }
```

## `magnet` as local CLI

A `magnet` CLI will the primary way for people to interact with the package manager.
It will be a binary which need to be downloaded and installed by the user and will currently not be part of the SuperCollider distribution.

The magnet client mainly is used to

* create a new magnet project
* add dependencies to a magnet project and resolve its dependencies recursively
* set up the environment for an existing magnet project
* bundle packages for release
* publishing packages on the index

### Packaging

Once a project should be published it needs to be packaged and sent to the server for indexing.

```bash
magnet package
```

Since magnet knows via the package manifest which files make up the package, it will be put into a `.tar.gz` file and exported into the `dist` folder.

The naming convention for the `.tar.gz` file follows

```
{package name}-{semantic version}-{quark/lepton}-{abi tag}-{platform tag}.tar.gz
```

For platform-dependent binary packages, so Leptons and Gluons, magnet will guess the platform and architecture, but it is possible to specify this explicitly through CLI flags and environment variables for cross-compilation.
The ABI tag needs to be specified, but defaults to the latest ABI version.
Leptons and Gluons will also upload a package of the source code to the index and will consist of everything that is not excluded via a `.gitignore` file.

```
{package name}-{semantic version}-{quark/lepton}-source.tar.gz
```

Since Quarks are not bound to a platform, it will follow the [Python wheels convention](https://peps.python.org/pep-0491/#file-name-convention) of using `any` for `platform tag` and the minimum supported version of sclang as abi tag, so e.g. `sclang3-10-0` which will be deduced from the `magnet.toml` file.

For binary packages, the abi tag depends on the convention of scsynth/supernova/sclang.

* `scsynth`
* `supernova`

The platform tag is one of the following values

* `win_amd64`
* `win_arm64`
* `win32`
* `linux_x86_64`
* `linux_arm64`
* `macosx_arm64`
* `macosx_amd64`
* `macosx_universal`

Before uploading the asset to the index a user is expected to inspect its content since the package index only allows to add, never to delete.

#### Package kinds

A package can be one of 2 kinds, *quark* or *lepton* (aka server plugins).
In the future it would be possible to extend this e.g. with *gluon*, which would be a binary plugin for sclang.

The kind of package does not take part in the dependency resolution, there can only be one kind of package for a given name.
In case a future *gluon* package also requires a *lepton* package, these should be two separate magnet projects which depend on each other.

#### Valid package names

Package names are

* case-insensitive
* `_`/`-` insensitive

#### Missing binary

If a matching binary can not be obtained for the platform, the user will be asked how to continue.
The user can either decide to use a different version which provides binaries for the platform or the user can provide a path where the binary is located.

```toml
[overrides]
foo = { path = "../foo" }
```

Packages which refer to packages via local paths can not be published via the index!

### Publishing a package

After the magnet package has successfully been created, it can be uploaded to the package index via the client or the website.

```bash
magnet publish dist/mypackage-0.1.0-quark-none-any.tar.gz
```

The client will send the projects `magnet.toml` file to the server which will inspect if it is valid in context of the index and will, if valid, register it as a new project (if applicable) and add the new version (if applicable).

Once this version has been registered it is possible to add assets to this version.
The client will upload the referenced asset and also calculate its sha256 checksum which the server will verify and also add it to the package index.
If it is a binary package it is mandatory to first upload the `source` package - but the client will implicitly take care of this.

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

Social logins (i.e. login via e.g. GitHub) may be available.
OAuth2 will be used to authenticate CLI clients.
It will be possible to generate longer lasting tokens for a CI environment which will be passed via environment variables.

The website of the index itself also allows to administrate packages, but since a simple frontend is used we can get away with using a session/cookie based approach.

## `magnet-index`

Since dependency resolving is a NP hard problem it may be necessary to go through all available versions of a package, which may also have changing dependencies over its version history.
Although the server has this information available, it would be really slow and put a lot of strain on the server to request this information for every package and installation.

Instead, the server will insert some metadata for every accepted version and asset of package into a dedicated file for each package.
This file is part of a git repository, called `magnet-index`, which acts as a distributed database.
Only the `magnet-server` can write into this repository.

The magnet CLI will clone this repository which contains the list of available packages and update it occasionally.
Since this repository stores all available packages, versions and assets, is can be used to resolve the requested dependencies locally, which improves performance and reduces load on the server dramatically.

Cargo is following a similar approach - take a look at their index which is located at https://github.com/rust-lang/crates.io-index

### Yanking

Once an asset of a version has been published it is **not** possible to delete it since this would allow to break existing environments.
The package index is therefore write only!
Instead, it is possible to *yank* an asset - a yanked asset will be ignored during the resolving of future projects, but if a client requests this specific asset it will still be served (though with a warning and maybe manual intervention).

Also packages which violate the CoC will only by yanked from the index.

## Integration

Since there is already an existing solution, there needs to be not just some incentive to use it, but it also needs to be easily accessible to the user by integrating it into the ecosystem.

### sclang

Like `Quarks` there will be a new `Magnet` class which communicates with the magnet CLI if is available/installed.
This will hopefully become part of the standard library of 3.15 to make the setup process easier.

### scsynth / supernova

Since dependencies are stored locally instead of globally it is necessary to allow loading arbitrary folders as source for plugins within scsynth/supernova.
There maybe needs to be some work done in order to make this happen since currently these paths seem to be hardcoded.

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
