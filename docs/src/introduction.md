# Magnet

*Magnet* is a package manager and index for SuperCollider extensions which provides

* re-producible and isolated environments, such that sharing and archiving projects is convenient as possible, even across operating systems
* serving of binary files so installation of server plugins is part of the package
* proper dependency resolution of transient dependencies using the *PubGrub* algorithm
* backwards-compatibility with the Quarks ecosystem

Instead of separating the setup between the development and distribution of a Quark and a composition, one just thinks in environments and projects.

Beyond that, Magnet makes it possible to

* display docs in the web for all Quarks and search for conflicting Class names (or a missing class)
* use Quarks and server extensions (called *Leptons*) in a wasm environment and the
* bootstrapping of the whole local SuperCollider ecosystem through magnet (e.g. exporting the local as an interactive website).

This website acts as a guide on how to use it as user and developer, but also provides the reasoning behind decisions and also hosts the specification.
