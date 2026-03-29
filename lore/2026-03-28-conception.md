# Conception

I want to run Claude Code with `--dangerously-skip-permissions` without actually being dangerous. The idea is simple:
run it inside a Docker container where it can't do anything destructive to the host.

The project directory gets mounted read-only, so claude can read the code but can't modify files on the host filesystem.
`~/.claude` is mounted read-write so auth, session state, and settings persist across runs. A single long-lived
container sticks around — no rebuilding on every invocation, just `docker exec` into the existing one.

The tool itself should be dead simple to use. `claudecage` in a project directory and you're in.
`claudecage container init` to set up the container the first time. That's about it.

I'm not trying to make this general-purpose yet. It's built for my setup, my preferences. I'll use it for a while and
see what's actually needed before worrying about configurability or other people's environments. Keep it proportional.
