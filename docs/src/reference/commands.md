# Commands

A `magnet` CLI will the primary way for people to interact with the package manager.
It will be a binary which need to be downloaded and installed by the user and will currently not be part of the SuperCollider distribution.

The magnet client mainly is used to

* create a new magnet project
* add dependencies to a magnet project and resolve its dependencies recursively
* set up the environment for an existing magnet project
* bundle packages for release (see [Packaging](../guide/publishing.md#packaging))
* publishing packages on the index (see [Publishing a package](../guide/publishing.md#publishing-a-package))

`magnet add`, `remove` and `update` change the manifest and the lock file and install the resulting dependencies into `magnet_packages/` right away, which can be skipped via `--no-install` such that only the files get written.
`magnet install` sets up an existing project, e.g. after cloning it, by installing the dependencies of its lock file.
