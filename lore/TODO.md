# TODO

## Make the docker image configurable

Right now the image has my personal preferences baked in — specific packages (less, zip, unzip, imagemagick, dprint,
sapling, etc.) are unconditionally installed in the Dockerfile. If someone else wants to use claudecage, they get my
toolchain whether they want it or not.

The image should be configurable so that users can declare what packages and tools they want installed, without having
to fork the project or maintain a patch on top of it. The hard-coded package list should be removed (or moved into a
default config that's clearly optional).

This probably means something like a user-provided config file or build args that control what gets installed. The
details are TBD — the main point is that the current approach of encoding one person's preferences into the base image
doesn't scale to other users.
