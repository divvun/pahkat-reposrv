# pahkat-reposrv

## Development

To run locally:
1. Create a `Config.toml` based on the included `Config.toml.example` and fill it in with appropriate values.
2. `cargo run -- -c Config.toml`

When developing and needing to modify the repo contents, or otherwise do strange things, you can self-host locally with Caddy.

```bash
caddy reverse-proxy --to localhost:9000
```