# pahkat-reposrv

## Development

To run locally:
1. Create a `Config.toml` based on the included `Config.toml.example` and fill it in with appropriate values.
2. `cargo run -- -c Config.toml`

When developing and needing to modify the repo contents, or otherwise do strange things, you can self-host locally with Caddy.

```bash
caddy reverse-proxy --to localhost:9000
```

## Deploying a Release

The following steps will create a new release and immediately deploy it to the production server.

To deploy a new version:
1. Update the version number in `Cargo.toml` and commit your changes
2. `git commit -m "Bump to version 1.6.9"`
3. Create a tag with the same version you set in `Cargo.toml`, for example: `git tag 1.6.9`
4. Push the tag *before* pushing main:`git push 1.6.9`
5. `git push`

:warning: **Beware:** your git client might betray you. It's recommended when releasing a new version to follow the steps exactly as above from the command line.