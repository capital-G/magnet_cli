# The Magnet environment

Magnet tries to replace the git-based Quarks ecosystem (referred to as *legacy-Quarks* within this website) and will extend it by also covering the binary extensions for the server (which will be named *Leptons* within the magnet environment) and the future (SuperCollider 3.16?) language extensions, eventually called *Gluons*.

The need for a package manager seems to be particularly necessary since SuperCollider 3.15 will break the server plugin API, such that all server extensions will need to be re-compiled.
Ideally, while re-compiling, people can publish their plugins into the Magnet ecosystem.

## What is the magnet environment?

The magnet environment consists of

* A `magnet.toml` as a project manifest to make the context to run a project explicit, bundled together with a `magnet.lock` which tracks all transient dependencies
* A CLI client `magnet-cli` which is used to set up a project, manage its dependencies and takes care of bundling operations for publishing the project on the package index.
  By publishing a project on the index it becomes a package which can be included by other projects and packages as dependency using semantic versioning.
* A server `magnet-server` which takes care of authentication and indexing all available packages and serving the contents of the packages.
* A git repository `magnet-index` which acts as a distributed database for the package index. This will be updated by the server and used by `magnet-cli` to resolve the dependency tree and request packages from the server.
