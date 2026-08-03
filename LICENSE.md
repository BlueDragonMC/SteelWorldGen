# License

This repository contains code under two different licenses:

- **Everything outside `steel-provider/`** (the Java client and library in
  `java-client/`, the benchmark, and build tooling) is licensed under the
  **Apache License, Version 2.0**. See [LICENSE-Apache-2.0](LICENSE-Apache-2.0)
  or <https://www.apache.org/licenses/LICENSE-2.0.txt>.

- **Everything under `steel-provider/`** (the Rust crate, including the
  `steel-provider` executable that is compiled into it) is licensed under the
  **GNU Affero General Public License, Version 3** because it is derived from
  and links against [SteelMC](https://github.com/Steel-Foundation/SteelMC),
  which is AGPL-3.0. See [steel-provider/LICENSE-AGPL](steel-provider/LICENSE-AGPL)
  or <https://www.gnu.org/licenses/agpl-3.0.txt>.
