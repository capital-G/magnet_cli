# The package index `magnet-index`

Since dependency resolving is a NP hard problem it may be necessary to go through all available versions of a package, which may also have changing dependencies over its version history.
Although the server has this information available, it would be really slow and put a lot of strain on the server to request this information for every package and installation.

Instead, the server will insert some metadata for every accepted version and asset of package into a dedicated file for each package.
This file is part of a git repository, called `magnet-index`, which acts as a distributed database.
Only the `magnet-server` can write into this repository.

The magnet CLI will clone this repository which contains the list of available packages and update it occasionally.
Since this repository stores all available packages, versions and assets, is can be used to resolve the requested dependencies locally, which improves performance and reduces load on the server dramatically.

This index will be split, i.e. instead of a flat namespace with all its files, we will use an approach which organizes the packages by its first letter into a sub-folder, i.e. `foobar` would be placed in `f/foobar.jsonl`.
This uses the normalized name of the package as a basis, i.e. the normalized representation of `DDW_Common` is `ddw-common` within magnet (see [Valid package names](manifest.md#valid-package-names)).

Each file while then have a one-lined JSON for each release, which covers

```json
{"name": "foo", "version": "0.1.3", "kinds": ["quark"], "sclang": ">=3.13", "dependencies": {"bar":  "<0.2.0"}, "yanked": false, "artifacts": [{"kind": "source", "abi": "any", "platform": "any", "sha256": "def"}, {"kind": "quark", "abi":  "any", "platform": "any", "sha256":  "abc"}]}
```

`sclang` is the range of the package's manifest and is omitted if the manifest has none.

`kinds` lists the kinds of artifacts of the version, which may change between versions, and each artifact names its kind.

Legacy quarks can also be used as dependencies by providing

```json
{"baz": {"legacy-quark": true, "git": {"ref": "<git sha>", "url":  "<link-to-repo>"}}}
```

The source of a version is represented via

```json
{"kind": "source", "abi": "any", "platform": "any"}
```

Such an entry will be inserted by the index service as a single writer on this index and can be regenerated from the DB of the index.

## `config.json`

Additionally, each index needs to implement a `config.json` at the root which implements

```json
{"index-format": 1, "dl": "<url template>", "http": "<base url>", "api": "<server url>"}
```

`api` is the URL of the `magnet-server` which belongs to this index, used to log in and publish, such that the URL of the index is the only thing which needs to be configured.
It can be overridden via `MAGNET_API_URL` / `--api-url`, e.g. for a local test server.

such that updates to the index can be made and also the download url can be changed in one position in case the packages need to migrate to a different URL.
The URL template needs to be configured in such a way that each artifacts results in a unique and stable URL: a version has at most one artifact per kind, abi and platform, and artifacts are never replaced.
If the template changes, the old URLs are redirected to the new ones.
The following pattern should be used such that people can download artifacts manually:

```
https://static.magnet.example/dl/{name}/{version}/{name}-{version}-{kind}-{abi}-{platform}.tar.gz
```

where `name` is the normalized name, `version` is used from the chosen line and `kind`, `abi` and `platform` from the selected artifact.

## HTTP mirror

A browser can not clone a git repository, so the server also publishes the files of the index as static files (`config.json` and `<letter>/<name>.jsonl`), similar to the sparse index of Cargo.
The optional `http` field points to the base URL of this mirror, and can be overridden via `MAGNET_HTTP_INDEX_URL` / `--http-index-url`, e.g. for a local test server.
The git repository stays the source of truth, and if the mirror moves, its old URLs are redirected like the ones of `dl`.

Beyond that, the server also provides an API to obtain more information (e.g. summary, dl-count, authors, readme, tages) about a given package and its version.
The git index stores the bare minimum for dependency resolution and how to obtain the downloadable artifact, such that the footprint of the local git index on each device is kept to a minimum.

> Cargo is following a similar approach - take a look at their index which is located at <https://github.com/rust-lang/crates.io-index>
