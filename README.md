# xmip-core-resilience-rate-limit

Rate limit guard: a token bucket that refills one permit per interval up to a burst, and makes an attempt wait for its token rather than refusing it. A technology of [xmip-core-resilience](https://github.com/IlleNilsson/xmip-core-resilience).

## Toolchain

`rust-toolchain.toml` pins the toolchain for the whole estate. Do not change it
here.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
