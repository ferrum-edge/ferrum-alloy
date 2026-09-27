# Swagger UI 5.33.0 (vendored)

The `openapi-ui` feature of `ferrum-alloy` embeds these files, unmodified,
from the npm package `swagger-ui-dist`. They are compiled into the binary;
nothing is fetched at build time or at runtime.

| | |
|---|---|
| Package | `swagger-ui-dist` 5.33.0 |
| Source | <https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-5.33.0.tgz> |
| npm `dist.integrity` | `sha512-wpdK+m6BU5yj6pmUdMskZVTSWYG4DLglAx3sIhylloY37i8O37IrH+YEpqdXNfpaTGxILRBFzUqLF2jKqbfI7A==` |
| npm `dist.shasum` (SHA-1) | `db69b90adfa96e1d5ac2f40d1cdc31f3dc663780` |
| Tarball SHA-256 | `434c69385aa02154348e6dcce0076df3a25ed88f673ac16cf4fed3fcf62c3b1b` |
| Upstream | <https://github.com/swagger-api/swagger-ui> |
| License | Apache-2.0 (`LICENSE`, `NOTICE`); bundled third-party notices in `swagger-ui-bundle.js.LICENSE.txt` |

The tarball matched the registry's `dist.integrity` before extraction.
`SHA256SUMS` lists the SHA-256 of every other file in this directory, and the
`openapi_ui_assets_match_the_manifest` test in
`crates/ferrum-alloy/tests/openapi_ui.rs` checks the served bytes and these
files against it.

Only these files are taken from the package:

- `swagger-ui-bundle.js`: Swagger UI with its dependencies, loaded by the page.
- `swagger-ui.css`: its stylesheet.
- `swagger-ui-bundle.js.LICENSE.txt`: the licenses of the dependencies inside the bundle (MIT, BSD-3-Clause, and DOMPurify under Apache-2.0 or MPL-2.0).
- `LICENSE` and `NOTICE`: Swagger UI's license and notice.

The package's `index.html` and `swagger-initializer.js` are not used. Alloy
serves its own page and initializer (`crates/ferrum-alloy/src/openapi_ui/`),
which load nothing inline and nothing from another origin. The package's
`@scarf/scarf` dependency is an npm install hook; it is not part of these files
and never runs.

## Updating

1. Read the version's `dist.integrity` from
   `https://registry.npmjs.org/swagger-ui-dist`.
2. Download the tarball, check its SHA-512 against `dist.integrity`, and
   extract it.
3. Copy the files above into a new `assets/swagger-ui/<version>/` directory,
   regenerate `SHA256SUMS` with `shasum -a 256`, update this table, and
   point the `include_bytes!` paths in `src/openapi_ui.rs` and the test at the
   new directory. Remove the old directory.
4. Review the bundle for new external URLs, `eval`, or inline styles that the
   page's Content-Security-Policy would block.
