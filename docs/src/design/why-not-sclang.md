# Why not sclang

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

## git integration

Although magnet includes a git client and its project structure is suited to be managed via git, magnet tries to follow the Unix approach and will therefore not cover any `git` management of the project.
People are encouraged though to use a git repository and tag the commit which they used to upload to the index.
