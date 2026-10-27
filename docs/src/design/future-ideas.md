# Future ideas

Things that become possible with magnet but are currently outside the scope of a v 1.0.

## Installing SuperCollider via `magnet` CLI

The `magnet` CLI could also install SuperCollider if no SuperCollider can be found.
This also allows to manage multiple SuperCollider versions for each project.
This would be really interesting for RaspberryPi and Linux (though it is really hard there w/o static builds).

See [pyenv](https://github.com/pyenv/pyenv) or [rustup](https://rust-lang.github.io/rustup/concepts/index.html) for this.

## Hosting documentation

Similar to <https://docs.rs/> we could also build and host the documentation of each package.
This should be a dedicated service, but this only becomes possible if we have a package index through which we know what kind of packages exist in the first place.

## Indexing classes and methods

Namespace collisions are really annoying and magnet has no way to avoid it.
Yet it would be possible to track all available classes and their methods, so during upload.

This is a somehow harder task since it either requires a sclang runtime on the client and server side (which introduces security risks since we are executing arbitrary code on the server, so this would need to be run in a strong isolation) or require a lightweight parser which does not evaluate the code.

## Workspaces and features

Cargo has a great feature called workspaces, see <https://doc.rust-lang.org/cargo/reference/workspaces.html>.
This allows to have meta-packages which are really great for mono-repos, such as `sc3-plugin`.
Once we have workspaces available, we could split up `sc3-plugins` into their own magnet projects, but `sc3-plugin` as a meta-package would guarantee that they are compatible with each other.

## Ideas regarding backwards compatibility

Currently, there is always much discussion involved if we actually need to change existing behavior within the class library or server plugins (we most of the time only do this when we come across a faulty implementation).

With having a reliable package manager it would be possible to fix the faulty behavior in a new version without breaking existing code b/c the old projects would still reference the old package.
This would be really impactful if we make the standard library a workspace meta-package since this would allow to develop drop-in replacements of existing code w/o breaking old code.

## Build system

Serving binary files is desirable over having to build everything oneself, but in case the binaries are missing for a given platform or ABI it should be possible to build the binaries from the given source via e.g. CMake.
For further information see

* Python:[`pyproject.toml` build-system](https://packaging.python.org/en/latest/guides/writing-pyproject-toml/#declaring-the-build-backend)
* Python: [`setuptools`](https://setuptools.pypa.io/en/latest/)
