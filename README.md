# Gowe (Rust)

Rust implementation of the Gowe wire format and session-aware encoder/decoder.

## What this crate provides

- Dynamic encoding/decoding (`encode`, `decode`)
- Schema-aware encoding (`encode_with_schema`)
- Batch and micro-batch encoding (`encode_batch`, `SessionEncoder::encode_micro_batch`)
- Stateful features (base snapshots, state patch, template batch, control stream, trained dictionary)

## Requirements

- Rust stable (edition 2024)

## Install

Add one of the following to `Cargo.toml`.

From GitHub:

```toml
[dependencies]
gowe = { git = "https://github.com/gowe-team/gowe-rust.git" }
```

From crates.io (if/when published):

```toml
[dependencies]
gowe = "0.1"
```

From a local path:

```toml
[dependencies]
gowe = { path = "./gowe-rust" }
```

## Quick start

```rust
use gowe::{decode, encode, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let value = Value::Map(vec![
        ("id".to_string(), Value::U64(1001)),
        ("name".to_string(), Value::String("alice".to_string())),
    ]);

    let bytes = encode(&value)?;
    let decoded = decode(&bytes)?;
    assert_eq!(decoded, value);
    Ok(())
}
```

## Session encoder example

```rust
use gowe::{create_session_encoder, SessionOptions, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut enc = create_session_encoder(SessionOptions::default());

    let value = Value::Map(vec![
        ("id".to_string(), Value::U64(1)),
        ("role".to_string(), Value::String("admin".to_string())),
    ]);

    let _bytes = enc.encode(&value)?;
    Ok(())
}
```

## Development

Run checks locally:

```bash
cargo fmt --all
cargo test
```

## License

See `LICENSE`.
