# Publishing

## Packaging

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
A package keeps the relative layout of the selected files, so it mirrors the project (see [Contents of package folders](projects.md#contents-of-package-folders)), since legacy quarks load resources relative to their class files and SCDoc relies on the folder structure of `HelpSource`.
Only the binaries of `platform-paths` are moved into `plugins/`.
A file which is selected by multiple globs of `global-paths` is only packaged once, but packaging fails if a file is selected by `global-paths` and `platform-paths`, or if a glob does not select any file, since this is probably a typo and a published artifact can never be replaced.

Every artifact also contains the `magnet.toml` of the project at its root, such that the server reads the manifest from the uploaded artifact (see [Checks of the server](#checks-of-the-server)), and the readme file of `[metadata]` (`README.md` by default, if it exists) at its path.
A quark has no dedicated source package, since its artifact already is the source.

The naming convention for the `.tar.gz` file follows

```
{name}-{version}-{kind}-{abi}-{platform}.tar.gz
```

where `name` is the normalized package name, `kind` the kind of the package (`quark`, `lepton` or `gluon`) and `abi` and `platform` the tags of the artifact (see [ABI tag](leptons.md#abi-tag) and [Platform tag](leptons.md#platform-tag)).
These filenames are only ever built from the fields of the index (see `config.json` of [`magnet-index`](../reference/index-format.md)) and never parsed, so a `-` within a name is not a problem.
Tags use `_` instead of `-` and only consist of `a-z`, `0-9` and `_`, so they are URL-safe.

For platform-dependent binary packages, so Leptons and Gluons, magnet will detect the platform and the ABI of the found SuperCollider, but it is possible to specify both explicitly through CLI flags and environment variables, e.g. for cross-compilation.
Leptons and Gluons will also upload a package of the source code to the index, which consists of the files selected by `source-paths`.

```
{name}-{version}-{kind}-source-any.tar.gz
```

Since Quarks are not bound to a platform or a binary interface, they use `any` for both tags, e.g. `foo-bar-1.2.0-quark-any-any.tar.gz`.
The supported SuperCollider versions are not a tag but the `sclang` range of the manifest (see [SuperCollider version](dependencies.md#supercollider-version)), like Python wheels keep `Requires-Python` in their metadata.

The archive is a gzip compressed tar file in the plain *ustar* format without GNU or pax extensions, such that a small reader is enough to unpack it, e.g. within a browser.
Long paths use the `prefix` field of ustar (155 characters for the directory in addition to the 100 characters of the name), and packaging fails for longer paths.
It only contains regular files and directories, and its entries are sorted and written with a fixed modification time, uid/gid 0 and mode 0644 (0755 for directories and executables), such that packaging the same files always results in the same archive.

Before uploading the asset to the index a user is expected to inspect its content since the package index only allows to add, never to delete.

## Publishing a package

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

### Checks of the server

The client packages the artifacts, but the server does not trust anything the client claims:

* The server computes the sha256 checksum of an artifact itself, and the index uses this value. The checksum sent by the client only guards the transfer and needs to match.
* The manifest is read from the `magnet.toml` within the artifact (see [Packaging](#packaging)), not from a separately sent copy, so the index and the contents of an artifact can not disagree. All artifacts of a version need to agree on the fields which are written into the index, i.e. name, version, kind, `sclang` and dependencies, and dependencies on magnet packages need to exist in the index.
* The archive is inspected but never extracted: the server streams through the tar headers without writing any files, and checks the archive format (see [Packaging](#packaging)), that all paths are relative and unique (also when ignoring the case), and that they are selected by the globs of the manifest.
* Against gzip bombs, the size of an upload is limited, and while streaming, the unpacked size, the number of entries and the length of the paths are counted, such that the inspection stops as soon as a limit is reached.

These checks only make sure that a package is well-formed, its contents, e.g. code within `*initClass` or the binaries, are not checked.

## Yanking

Once an asset of a version has been published it is **not** possible for the user to delete it since this would allow to break existing environments.
The package index is therefore append only!
Instead, it is possible to *yank* a version - a yanked version will be ignored during the resolving of future projects, but if a client requests this specific asset it will still be served (though with a warning and maybe manual intervention).
The owners of a package can yank a version, and undo this, via the website.
Projects which already have a yanked version in their lock file keep it, and `magnet install` warns about it.

## Deletion

Yanking is the normal way to retract a version, but some cases require to actually delete a version or a whole package:

* violations of the CoC
* malware
* legal takedowns
* leaked secrets

This is only an index-admin operation.
A deletion removes the version from the DB of the server, since otherwise regenerating the index from the DB would bring it back, and its artifacts from the storage.
The version number (or the name of a deleted package) can never be used again, unless an admin frees the name, such that a lock file can never refer to different contents under the same version.
Downloads of a deleted artifact answer with `410 Gone`, so lock files which refer to a deleted version fail to install with a clear error and need to be resolved again via `magnet update <name>`.

## Trust

The checks of the server (see [Checks of the server](#checks-of-the-server)) only make sure that a package is well-formed.
Installing a package means trusting its authors: the classes of a quark are executed by sclang (e.g. within `*initClass`), and the binaries of leptons and gluons are arbitrary native code which runs with the permissions of the user.
Signing artifacts and trusted publishing (i.e. publishing only from a CI of the package's repository, without long-lived tokens) are not covered yet.
