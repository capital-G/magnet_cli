# Dependencies

## Semantic versioning

Packages need to define their versions using [semantic versioning](https://semver.org/) which defines for a clear ordering and also communicate compatibility issues.

Since magnet can only provide a reliable infrastructure, it depends on its users to actually build a reliable package environment!
Therefore, some advice:

* Always add, never remove any exposed API! I.e. do not change or remove class names, methods, argument names or positions. `pr` scoped methods act as the only exemption.
* Prefer keyword arguments over positional arguments to be more resilient towards API changes of other packages. This introduces a performance overhead, but this is probably tolerable for having more resilient code.
* Treat bumping a major version like renaming the package since it is unclear if previous code which relied on the package still works. Use versions `<1.0` to indicate that this package has still an unstable API.

At some point it will be hopefully possible to indicate a version as `alpha`, `beta` or `rc` in order to publish a version for testing which will be ignored when resolving dependency unless explicitly requested.

### Version range restriction

Version restrictions follow the ones from [`cargo`](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html) and [`npm`](https://docs.npmjs.com/cli/v12/configuring-npm/package-json#dependencies)

Written | Comment
--- | ---
`1.2.3` | Equivalent to `^1.2.3`
`^1.2.3` | `>=1.2.3`, `<2.0.0`
` ` (empty) | Equivalent to `*`
`*` | `>=0.0.0`
`~1.2.3` | `>=1.2.3`, `<1.3.0`

Pre-releases are not supported yet.

### SuperCollider version

The `sclang` entry of `[package]` states which versions of SuperCollider a package works with, using the version ranges from above, e.g. `sclang = ">=3.13"`.
This applies to every package, whatever kinds of artifacts it contains.

While resolving, a version of a dependency is only chosen if its `sclang` range allows the lower bound of the project's `sclang` range, so the lock file holds for the whole range of the project, like it holds across platforms.
Older versions of a dependency are tried if a newer one requires a more recent SuperCollider.
Like [uv](https://docs.astral.sh/uv/) does for `requires-python`, upper bounds of dependencies are ignored, since defensive caps would block the resolution, so packages should only declare a lower bound.
An upper bound only counts on the root project.

If the project declares no `sclang` range, the dependencies are resolved without it and magnet warns if a locked package requires a newer version than the sclang found on the system.
`magnet init` writes `>=<major>.<minor>` of the found sclang.

The SuperCollider version belongs to a package version and not to its artifacts, so it is not part of the filename of an artifact (see [Packaging](publishing.md#packaging)): the resolver picks versions and the artifacts are only picked afterwards, so a tag would fail at install instead of falling back to an older compatible version.

## Installing legacy Quark dependencies

Backwards compatibility is valued highly in SuperCollider, so a new package manager should not diverge from this credo.
Therefore, it is possible to depend on existing legacy Quarks (so projects which have a `*.quark` file in a git repository but do not have a `magnet.toml` and not present in the `magnet` index) by refering to them either via their name or the location of their git repository, which mimicks the legacy Quarks behavior.
The dependencies of a legacy quark will be tried to be extracted from its `.quark` file through a custom parser and if the dependency exists as magnet package, it will be prefered over installing the dependency via the legacy quark system since magnet allows to resolve version conflicts which the legacy quark system is not capable of.

All names of the legacy Quarks that are tracked in the [current index](https://github.com/supercollider-quarks/quarks/blob/master/directory.txt) are blocked in the new index such that [name-squatting](https://peps.python.org/pep-0541/) on existing projects can not be performed.
This is also the reason why magnet index is relying on a [flat namespace](https://samsieber.tech/posts/2020/09/registry-structure-influence/).
Only authors and maintainers of those existing packages can take the name in the new magnet index, though this can be also decided on a case-by-case basis if the project is abandoned.

Since the versioning of Quarks is not really existing, Quarks will be pinned by their commit hash in order to provide reproducible environments.
A legacy dependency uses the `tag`, `ref` or `branch` of the manifest, otherwise the refspec of its entry in `directory.txt`, otherwise the default branch, and the resulting commit is stored in the lock file until `magnet update` moves it.

Transitive dependencies of quarks will also be added as restrictions to the magnet dependency graph.
Since the legacy Quark system ignores their versions, a dependency of a `.quark` file becomes `*` if a magnet package with this name exists (also if it names a version or a refspec), and otherwise falls back to the legacy quark from `directory.txt`.
If a `.quark` file is not a plain literal (e.g. it contains code), magnet warns and treats it as having no dependencies.
In this case the dependencies can be listed by hand in the manifest together with `ignore-quark-deps = true`.
A package which is required from two different sources (e.g. as a legacy quark and as a magnet package) is an error, which can be resolved via `[overrides]`.

In order to provide a smooth transition, published magnet packages can depend on those legacy quarks, though only via a pinned commit hash or tag, not via a branch.
