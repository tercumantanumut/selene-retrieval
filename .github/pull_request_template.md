## What and why

<!-- What does this change, as a user would see it? Why is it needed? Link the issue if there is one. -->

## How it was tested

<!-- Commands you ran, and what you checked by hand. -->

## Checklist

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo deny --locked check`
- [ ] UI changed: `npm run lint`, `npm test` and `npm run build` in `ui/`
- [ ] Can an average user do this without documentation?
- [ ] Security defaults are unchanged.
