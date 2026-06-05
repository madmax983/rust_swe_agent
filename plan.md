1. **The Tangle**: Currently `ArtifactSchemaError`, `WebhookSinkError`, `SweepWebhookSinkError`, `BundleError` and perhaps a few other minor errors implement `std::error::Error` or use `#[derive(thiserror::Error)]` without being part of the central `crate::error::Error` enum.
2. **The Blueprint**: We will unify error handling into the central `crate::error::Error` enum.
We will add `ArtifactSchema(#[from] crate::artifact::ArtifactSchemaError)`, `Bundle(#[from] crate::run::bundle::BundleError)`, `Webhook(#[from] crate::stream::webhook::WebhookSinkError)`, and `SweepWebhook(#[from] crate::stream::sweep_webhook::SweepWebhookSinkError)` into `crate::error::Error`.
Also, we need to fix any places in the code that will break if we modify these.
Wait, let's check if there are other isolated `thiserror::Error` types. `grep -rn "derive.*thiserror::Error" src/` gave us exactly these 4 enums:
- `ArtifactSchemaError` in `src/artifact.rs`
- `WebhookSinkError` in `src/stream/webhook.rs`
- `SweepWebhookSinkError` in `src/stream/sweep_webhook.rs`
- `BundleError` in `src/run/bundle.rs`
Wait, are there any more? Let's check `grep -rn "thiserror::Error" src/`.
- `src/artifact.rs:12`
- `src/stream/webhook.rs:72`
- `src/stream/sweep_webhook.rs:193`
- `src/run/bundle.rs:40`
- `src/error.rs:4`

This confirms there are exactly 4 separate error types outside `src/error.rs` using `thiserror`.
We will add them as variants to `crate::error::Error` in `src/error.rs`.

```rust
    #[error(transparent)]
    ArtifactSchema(#[from] crate::artifact::ArtifactSchemaError),

    #[error(transparent)]
    Bundle(#[from] crate::run::bundle::BundleError),

    #[error(transparent)]
    Webhook(#[from] crate::stream::webhook::WebhookSinkError),

    #[error(transparent)]
    SweepWebhook(#[from] crate::stream::sweep_webhook::SweepWebhookSinkError),
```

Wait, `crate::error::Error` does not use these types currently. Are there places where they are returned that we should change to `Result<T, crate::error::Error>` instead?
Let's check where they are used.
`ArtifactSchemaError` is returned by `src/artifact.rs`.
`WebhookSinkError` is returned by `WebhookSink::new`.
`SweepWebhookSinkError` is returned by `SweepWebhookSink::new`.
`BundleError` is returned by `src/run/bundle.rs` functions.

Let's just integrate them into `crate::error::Error`.
Wait, what if `BundleError` has conflicting `#[from]` inside it?
`BundleError` has:
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
`crate::error::Error` also has `Io` and `Json`.
If we have `Bundle(#[from] BundleError)` in `crate::error::Error`, there's an ambiguity if we do `?` on `io::Error` in a function returning `Result<T, crate::error::Error>`?
No, the compiler will use the direct `From<io::Error> for Error` if we're converting `io::Error` to `Error`. If we're converting `BundleError` to `Error`, it will use `From<BundleError> for Error`.

Wait, Atlas directive says: "Standardize error types across modules (e.g., Result<T, crate::Error>)."
By wrapping these subsystem errors into `crate::error::Error`, we unify the error type.
Let's see if we can refactor `BundleError`, `ArtifactSchemaError`, etc. to just be part of the central Error, OR if we keep them as separate enums and use `#[from]` in `crate::error::Error`.
The `src/error.rs` doc comment says:
"Error types. One top-level `Error` composed of focused subsystem enums, so call sites can match on the specific failure mode without a catch-all."
So keeping them as separate enums but wrapping them in `crate::error::Error` is the EXACT right pattern for this codebase!

3. **Execution**:
   - Add the missing `#[error(transparent)]` subsystem variants to `src/error.rs`.
   - Update tests to ensure `cargo check` passes.
