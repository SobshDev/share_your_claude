# Third-party notices

Shared Router is MIT licensed (see [LICENSE](LICENSE)). The `shared-router` binary and Docker image also contain third-party software, listed here:

- Rust crates statically linked into the binary, with their license texts, in [THIRD_PARTY_NOTICES_CRATES.md](THIRD_PARTY_NOTICES_CRATES.md). That file is generated from `Cargo.lock` with `cargo about`; see [about.toml](about.toml).
- SQLite, bundled into the binary through the `libsqlite3-sys` crate, below.
- Code adapted from opencodex, below.

The Debian base image carries its own package notices under `/usr/share/doc`.

## SQLite

The binary compiles in the SQLite 3.46.0 amalgamation shipped with `libsqlite3-sys` 0.30.1. SQLite is in the public domain. Its source carries this notice in place of a license:

```text
The author disclaims copyright to this source code.  In place of
a legal notice, here is a blessing:

   May you do good and not evil.
   May you find forgiveness for yourself and forgive others.
   May you share freely, never taking more than you give.
```

Source: https://sqlite.org/copyright.html

## opencodex

The Anthropic OAuth flow constants, PKCE exchange behavior, OAuth beta headers and system compatibility instruction are adapted from opencodex 2.49.0, particularly `src/oauth/anthropic.ts`, `src/adapters/anthropic.ts`, and `src/adapters/client-fingerprint.ts`.

Source: https://github.com/lidge-jun/opencodex

The upstream license is MIT. The full copyright and license notice from the installed version is reproduced below.

MIT License

Copyright (c) 2026 opencodex contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
