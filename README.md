# xmip-core-resilience-rate-limit

Rate limit guard: a token bucket that refills one permit per interval up to a burst, and makes an attempt wait for its token rather than refusing it. The bucket refills on the Clock it is handed (`xcore::Clock`). A technology of [xmip-core-resilience](https://github.com/IlleNilsson/xmip-core-resilience).

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
