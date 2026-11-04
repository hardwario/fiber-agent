# Vendored JS

Downloaded once and committed — no bundler, no npm, no node in the viewer
path (see plan/viewer/stack.md rule #4). To bump a version: download the new
file over the old one, update the version + hash below.

| File | Library | Version | Source | sha256 |
|---|---|---|---|---|
| `htmx.min.js` | htmx | 2.0.9 | `github.com/bigskysoftware/htmx` release `v2.0.9` (`htmx.min.js` asset) | `57d9191515339922bd1356d7b2d80b1ee3b29f1b3a2c65a078bb8b2e8fd9ae5f` |
| `htmx-sse.js` | htmx SSE extension | commit `82db5bce` (2025-05-12) | `htmx-extensions` has no tagged releases; pinned to the commit that last touched `src/sse/sse.js`: `raw.githubusercontent.com/bigskysoftware/htmx-extensions/82db5bce51d2a255f0bcc6b93da5dd35130a296d/src/sse/sse.js` | `3b5992a541619babefc4c169505af474df5c3039da51e59b96ccf9241ecd61d2` |
| `alpine.min.js` | Alpine.js | 3.16.2 | Alpine has no GitHub release binaries; pulled from the npm-mirroring CDN: `cdn.jsdelivr.net/npm/alpinejs@3.16.2/dist/cdn.min.js` | `c13d85e590cf4c91959aeaaef1ba755344e911d55c5ec56f472ed2894e2b4684` |
| `echarts.min.js` | Apache ECharts | 5.6.0 | Same story as Alpine (npm-only): `cdn.jsdelivr.net/npm/echarts@5.6.0/dist/echarts.min.js` | `bf4a223524e40b77c304bec67e1222cf551f14880cf42c69dc046558e11c07b1` |
| `cally.js` | Cally | 0.9.2 | npm-only, same as Alpine: `cdn.jsdelivr.net/npm/cally@0.9.2/dist/cally.js` (ESM, ships unminified) | `4dcba6a3b8ec63b66ff2fbfc6cf47b7dee1eb4896eac89fe7487d82e28e15870` |

Verify after any re-download:

```sh
shasum -a 256 static/js/*.js
```
