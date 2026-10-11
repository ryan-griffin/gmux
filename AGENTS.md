# Working on Gmux

- Performance, memory usage, and rendering quality are top priorities. Strive for the optimal implementation. Measure changes that affect performance or memory usage.
- Consider all affected parts when making a change: UI, CLI and socket API, daemon, terminal engine,
  rendering, saved state, etc.
- Keep code clean and maintainable. Do not preserve complexity just because it already exists. Work towards the best solution even if refactors are required.
- Tests should earn their place. Protect meaningful behavior and catch regressions. Avoid duplicate coverage and trivial tests.
- For UI changes, consult the [GNOME HIG](https://developer.gnome.org/hig/) and existing app conventions.
- Gmux is pre-alpha. Do not maintain backwards compatibility unless explicitly requested.
- Keep docs focused on current behavior and usage, and the README useful to
  users. Leave out progress journals and debugging transcripts.
