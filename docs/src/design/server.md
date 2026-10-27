# `magnet-server`

The magnet server keeps track of all available packages and manages authentication of users when they publish a new version of a package through the CLI.
It also stores all the assets of each package and servers them to a client during installation of dependencies.

The server is written in Django and provides a basic frontend and a REST API to interact with it.
Ideally there will be two available instances of this server (and index): One for production and one for testing.

## Storage

Since it is unclear how big the storage of such a project would become over time I decided to directly go with an object storage solution rather than having a local-only approach which later need to be adapted to a distributed one.

Since MinIO has become unusable, I settled for [Garage](https://garagehq.deuxfleurs.fr/) to have a local S3-environment, but this may be later replaced by a S3-like service.

## Authentication

Social logins will likely not be made available.
OAuth2 will be used to authenticate CLI clients.
`magnet login` opens the authorization flow (with PKCE, since the CLI can not keep a client secret), stores the token in the magnet home folder for each server and refreshes it when it expires, and `magnet logout` revokes it.
It will be possible to generate longer lasting tokens for a CI environment on the website, which are limited to publishing and will be passed via the `MAGNET_TOKEN` environment variable.
Such a token is sent like any other OAuth2 token and is never written to disk by the client.

The website of the index itself also allows to administrate packages, but since a simple frontend is used we can get away with using a session/cookie based approach.
