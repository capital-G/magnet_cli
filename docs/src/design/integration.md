# Integration

Since there is already an existing solution, there needs to be not just some incentive to use it, but it also needs to be easily accessible to the user by integrating it into the ecosystem.

## sclang

Like `Quarks` there will be a `Magnet` class which communicates with the magnet CLI.
magnet provides it within magnet projects by generating it into `magnet_packages`, like the class for the server plugins, so it gets released together with magnet and always matches the CLI which installed the project, instead of being part of the standard library of SuperCollider.

## scsynth / supernova

Since dependencies are stored locally instead of globally it is necessary to allow loading arbitrary folders as source for plugins within scsynth/supernova.
This is already possible via `ServerOptions.ugenPluginsPath`, though it requires to also list the plugins of SuperCollider itself (see [Server plugins](../guide/leptons.md#server-plugins)).

## sc-ide

Instead of just opening files, there should be a way to also open a project which sets up the IDE accordingly.
Maybe it is possible to extend the already existing Session view for this, since sc-ide always have one implicit interpreter - therefore it should be avoided to be able to switch between different interpreters (global one, magnet-scoped one) in order to avoid confusion.

There is also a file explorer in the works for the IDE (see <https://github.com/supercollider/supercollider/pull/6549>) which would help navigation within a magnet project.

There should also be a way to open a terminal within the IDE which either opens the current working directory of sclang or the root of a magnet project.

## sc3-plugins

Since sc3-plugins is maintained by the SC developers and has become somehow a default extension for SC, it can act as a great way to test out the magnet packaging system.
For 3.15, sc3-plugins would be additionally distributed via a magnet package.
